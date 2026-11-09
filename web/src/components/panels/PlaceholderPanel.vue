<script setup lang="ts">
import SkeletonBlock from "../SkeletonBlock.vue";
import type { AppContract } from "../../stores/apps.ts";

/**
 * What a window shows until its app has a backend.
 *
 * It is a skeleton plus one honest line, not a fake UI. A mocked file list
 * would make the shell look finished and hide which apps actually exist, and
 * the first rule of the project is that every interface is real.
 */
const props = defineProps<{
  app: AppContract;
  /** False once the panel's own backend is answering. */
  loading: boolean;
  /** Set when the bus is not connected, which is the usual reason. */
  reason?: string;
}>();
</script>

<template>
  <div class="panel" role="group" :aria-label="props.app.title" :aria-busy="props.loading">
    <div class="rows">
      <div v-for="row in 7" :key="row" class="row">
        <SkeletonBlock width="18px" height="14px" radius="var(--dj-radius-xs)" />
        <SkeletonBlock :width="`${38 + ((row * 13) % 44)}%`" height="12px" />
      </div>
    </div>
    <p class="note">
      <strong>{{ props.app.title }}</strong> has no backend on this node yet.
      {{ props.reason ?? "Its panel renders as soon as the app registers its commands on the bus." }}
    </p>
  </div>
</template>

<style scoped>
.panel {
  display: flex;
  flex-direction: column;
  flex: 1;
  min-height: 0;
  padding: var(--dj-space-3);
  gap: var(--dj-space-3);
  background: var(--dj-color-surface);
}

.rows {
  display: flex;
  flex-direction: column;
  flex: 1;
  min-height: 0;
  gap: var(--dj-space-1);
  overflow: hidden;
}

.row {
  display: flex;
  align-items: center;
  gap: var(--dj-space-3);
  min-height: var(--dj-row);
  padding-inline: var(--dj-space-2);
  border-bottom: var(--dj-stroke-hairline) solid var(--dj-color-hairline);
}

.note {
  flex: none;
  color: var(--dj-color-text-muted);
  font-size: var(--dj-type-small-size);
  line-height: var(--dj-type-small-line);
}

.note strong {
  color: var(--dj-color-text);
  font-weight: var(--dj-type-strong-weight);
}
</style>
