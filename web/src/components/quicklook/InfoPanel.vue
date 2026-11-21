<script setup lang="ts">
import { computed, ref, watch } from "vue";

import { useShell } from "../../shell/context.ts";
import { EXIF_HEAD_BYTES, readJpegMetadata, type Exif } from "../../shell/exif.ts";
import { contentUrl, formatBytes, formatDuration } from "../../shell/quicklook.ts";

/**
 * Facts about the file being previewed.
 *
 * Three sources, each the one that actually knows: the node's `files.stat`
 * for size and modification time; the viewer itself for what only decoding
 * reveals (dimensions, duration, page count, entries); and, for a JPEG, its
 * EXIF block, read from the first {@link EXIF_HEAD_BYTES} with a range
 * request and parsed here (see `exif.ts` for why not on the node).
 *
 * Fetched only while this panel is open, so swiping through a folder of
 * photographs does not cost an extra request per photograph unless the
 * operator asked to see the details.
 */
export interface ViewerMeta {
  width?: number;
  height?: number;
  duration?: number;
  pages?: number;
  entries?: number;
  format?: string;
  lines?: number;
}

const props = defineProps<{ path: string; kind: string; meta: ViewerMeta }>();

const shell = useShell();
const stat = ref<{ size?: number; modified?: number | null; readonly?: boolean } | null>(null);
const statError = ref<string | null>(null);
const exif = ref<Exif | null>(null);
const jpegSize = ref<{ width?: number; height?: number }>({});
let ticket = 0;

const isJpeg = computed(() => /\.(jpe?g|jfif)$/i.test(props.path));

async function load(path: string) {
  const mine = ++ticket;
  stat.value = null;
  statError.value = null;
  exif.value = null;
  jpegSize.value = {};

  const outcome = await shell.bus.call("files.stat", { path });
  if (mine !== ticket) return;
  if (outcome.status === "ok") {
    const v = outcome.value as { size?: unknown; modified?: unknown; readonly?: unknown };
    stat.value = {
      size: typeof v.size === "number" ? v.size : undefined,
      modified: typeof v.modified === "number" ? v.modified : null,
      readonly: v.readonly === true,
    };
  } else {
    statError.value = "message" in outcome ? outcome.message : "The node did not say.";
  }

  if (isJpeg.value) {
    try {
      const response = await fetch(contentUrl(path), {
        headers: { Range: `bytes=0-${EXIF_HEAD_BYTES - 1}` },
        credentials: "same-origin",
      });
      if (mine !== ticket || !response.ok) return;
      const parsed = readJpegMetadata(new Uint8Array(await response.arrayBuffer()));
      if (mine !== ticket) return;
      exif.value = parsed.exif;
      jpegSize.value = { width: parsed.width, height: parsed.height };
    } catch {
      // No EXIF is a missing row, not an error worth a banner.
    }
  }
}

watch(() => props.path, load, { immediate: true });

const KIND: Record<string, string> = {
  image: "Image",
  video: "Video",
  audio: "Audio",
  pdf: "PDF document",
  text: "Text",
  markdown: "Markdown document",
  archive: "Archive",
  web: "Web page or markup",
  none: "No preview",
};

const rows = computed(() => {
  const out: Array<[string, string]> = [];
  const m = props.meta;
  out.push(["Kind", m.format ?? KIND[props.kind] ?? props.kind]);
  if (stat.value?.size !== undefined) out.push(["Size", formatBytes(stat.value.size)]);
  if (stat.value?.modified) {
    out.push([
      "Modified",
      new Date(stat.value.modified * 1000).toLocaleString(undefined, {
        dateStyle: "medium",
        timeStyle: "short",
      }),
    ]);
  }
  if (stat.value?.readonly) out.push(["Access", "Read-only"]);
  const w = m.width ?? jpegSize.value.width;
  const h = m.height ?? jpegSize.value.height;
  if (w && h) out.push(["Dimensions", `${w} × ${h}`]);
  if (m.duration !== undefined && Number.isFinite(m.duration)) {
    out.push(["Duration", formatDuration(m.duration)]);
  }
  if (m.pages !== undefined) out.push(["Pages", String(m.pages)]);
  if (m.entries !== undefined) out.push(["Entries", m.entries.toLocaleString()]);
  if (m.lines !== undefined) out.push(["Lines", m.lines.toLocaleString()]);

  const e = exif.value;
  if (e) {
    // "NIKON CORPORATION" + "NIKON Z 6" reads as "NIKON Z 6"; a model that
    // does not repeat the maker gets the maker in front.
    const brand = (e.make ?? "").split(" ")[0]!.toLowerCase();
    const camera =
      e.model && brand && e.model.toLowerCase().startsWith(brand)
        ? e.model
        : [e.make, e.model].filter(Boolean).join(" ");
    if (camera) out.push(["Camera", camera]);
    if (e.lens) out.push(["Lens", e.lens]);
    if (e.taken) out.push(["Taken", e.taken]);
    const exposure = [e.exposure, e.fNumber, e.iso ? `ISO ${e.iso}` : undefined, e.focalLength]
      .filter(Boolean)
      .join("  ·  ");
    if (exposure) out.push(["Exposure", exposure]);
    if (e.gps) {
      out.push(["Location", `${e.gps.latitude.toFixed(5)}, ${e.gps.longitude.toFixed(5)}`]);
    }
  }
  return out;
});
</script>

<template>
  <aside class="info" aria-label="File details">
    <dl class="facts">
      <template v-for="[label, value] in rows" :key="label">
        <dt>{{ label }}</dt>
        <dd>{{ value }}</dd>
      </template>
    </dl>
    <p v-if="statError" class="quiet">{{ statError }}</p>
  </aside>
</template>

<style scoped>
.info {
  overflow: auto;
  padding: var(--dj-space-3);
  border-left: var(--dj-stroke-hairline) solid var(--dj-color-border);
  background: var(--dj-color-surface);
  min-width: 0;
}

.facts {
  display: grid;
  grid-template-columns: auto 1fr;
  gap: var(--dj-space-2) var(--dj-space-3);
  margin: 0;
}

dt {
  color: var(--dj-color-text-muted);
  font-size: var(--dj-type-micro-size);
  padding-top: 0.15em;
}

dd {
  margin: 0;
  overflow-wrap: anywhere;
}

.quiet {
  color: var(--dj-color-text-muted);
  font-size: var(--dj-type-micro-size);
}
</style>
