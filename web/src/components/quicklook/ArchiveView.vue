<script setup lang="ts">
import { computed, ref, watch } from "vue";

import { useShell } from "../../shell/context.ts";
import { formatBytes } from "../../shell/quicklook.ts";

/**
 * What is inside an archive, from the node's `files.archive.list`.
 *
 * The node reads the directory where the bytes are and returns names; nothing
 * is extracted, on the node or here. A listing that stopped short says so and
 * says why, because a partial list shown as the whole is how a missing file
 * gets assumed absent.
 */
const props = defineProps<{ path: string }>();
const emit = defineEmits<{ (e: "meta", meta: { entries: number; format: string }): void }>();

interface Entry {
  name: string;
  is_dir: boolean;
  is_link: boolean;
  size: number | null;
  modified: number | null;
  encrypted: boolean;
}
interface Listing {
  format: string;
  entries: Entry[];
  total_entries: number | null;
  truncated: boolean;
  stopped: string | null;
}

const shell = useShell();
const state = ref<
  { status: "loading" } | { status: "failed"; why: string } | { status: "ready"; listing: Listing }
>({ status: "loading" });
let asked = 0;

const FORMAT: Record<string, string> = {
  zip: "Zip archive",
  tar: "Tar archive",
  tar_gzip: "Gzipped tar archive",
  gzip: "Gzip-compressed file",
};

async function load(path: string) {
  const ticket = ++asked;
  state.value = { status: "loading" };
  const outcome = await shell.bus.call("files.archive.list", { path });
  if (ticket !== asked) return;
  if (outcome.status !== "ok") {
    state.value = {
      status: "failed",
      why: "message" in outcome ? outcome.message : "The node did not list this archive.",
    };
    return;
  }
  const listing = parse(outcome.value);
  if (!listing) {
    state.value = { status: "failed", why: "The node's answer was not a listing." };
    return;
  }
  state.value = { status: "ready", listing };
  emit("meta", {
    entries: listing.total_entries ?? listing.entries.length,
    format: FORMAT[listing.format] ?? listing.format,
  });
}

function parse(value: unknown): Listing | null {
  const v = value as Partial<Listing> | null;
  if (!v || !Array.isArray(v.entries)) return null;
  return {
    format: String(v.format ?? ""),
    entries: v.entries.filter((e): e is Entry => typeof e?.name === "string"),
    total_entries: typeof v.total_entries === "number" ? v.total_entries : null,
    truncated: v.truncated === true,
    stopped: typeof v.stopped === "string" ? v.stopped : null,
  };
}

watch(() => props.path, load, { immediate: true });

const summary = computed(() => {
  if (state.value.status !== "ready") return "";
  const l = state.value.listing;
  const shown = l.entries.length.toLocaleString();
  const kind = FORMAT[l.format] ?? l.format;
  if (l.truncated) {
    const of = l.total_entries !== null ? ` of ${l.total_entries.toLocaleString()}` : "";
    return `${kind}. Showing the first ${shown}${of} entries.`;
  }
  return `${kind}, ${shown} ${l.entries.length === 1 ? "entry" : "entries"}.`;
});

function when(seconds: number | null): string {
  if (seconds === null) return "";
  return new Date(seconds * 1000).toLocaleString(undefined, {
    dateStyle: "medium",
    timeStyle: "short",
  });
}
</script>

<template>
  <div class="archive">
    <p v-if="state.status === 'loading'" class="quiet" role="status">Reading the archive…</p>

    <div v-else-if="state.status === 'failed'" class="refusal" role="alert">
      <p class="headline">No listing</p>
      <p class="why">{{ state.why }}</p>
    </div>

    <template v-else>
      <p class="summary">{{ summary }}</p>
      <p v-if="state.listing.stopped" class="stopped" role="note">{{ state.listing.stopped }}</p>
      <table class="entries">
        <thead>
          <tr>
            <th scope="col">Name</th>
            <th scope="col" class="num">Size</th>
            <th scope="col" class="date">Modified</th>
          </tr>
        </thead>
        <tbody>
          <tr v-for="(e, n) in state.listing.entries" :key="n" :data-dir="e.is_dir ? '' : undefined">
            <td class="name">
              <span class="glyph" aria-hidden="true">{{ e.is_dir ? "▸" : e.is_link ? "↪" : "·" }}</span>
              {{ e.name }}
              <span v-if="e.encrypted" class="badge">encrypted</span>
            </td>
            <td class="num">{{ e.is_dir || e.size === null ? "" : formatBytes(e.size) }}</td>
            <td class="date">{{ when(e.modified) }}</td>
          </tr>
        </tbody>
      </table>
    </template>
  </div>
</template>

<style scoped>
.archive {
  width: 100%;
  height: 100%;
  overflow: auto;
  padding: var(--dj-space-3);
  background: var(--dj-color-raised);
  border-radius: var(--dj-radius-md);
  touch-action: pan-y;
}

.summary {
  margin: 0 0 var(--dj-space-2);
  color: var(--dj-color-text-muted);
}

.stopped {
  margin: 0 0 var(--dj-space-2);
  color: var(--dj-color-warning, var(--dj-color-text-muted));
}

.entries {
  width: 100%;
  border-collapse: collapse;
  font-size: var(--dj-type-small-size, 0.9rem);
}

th {
  text-align: left;
  font-weight: var(--dj-type-strong-weight);
  padding: var(--dj-space-1) var(--dj-space-2);
  border-bottom: var(--dj-stroke-hairline) solid var(--dj-color-border);
}

td {
  padding: var(--dj-space-1) var(--dj-space-2);
  vertical-align: top;
}

.name {
  font-family: var(--dj-font-mono);
  overflow-wrap: anywhere;
}

.glyph {
  display: inline-block;
  width: 1.2em;
  color: var(--dj-color-text-dim);
}

.num {
  text-align: right;
  white-space: nowrap;
}

.date {
  white-space: nowrap;
  color: var(--dj-color-text-muted);
}

.badge {
  margin-left: var(--dj-space-2);
  font-size: var(--dj-type-micro-size);
  color: var(--dj-color-text-muted);
}

/* On a phone the date column is the one to give up. */
@media (max-width: 520px) {
  .date {
    display: none;
  }
}

.quiet {
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
