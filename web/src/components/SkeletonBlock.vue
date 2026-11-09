<script setup lang="ts">
/**
 * A skeleton, which is what Deckjack shows while something loads.
 *
 * Spinners are banned by the design doc and this is why: a skeleton occupies
 * the box the content will occupy, so when the content lands nothing moves. A
 * spinner occupies a box of its own and every arrival is a layout shift.
 *
 * It is `aria-hidden` with a live label on the container instead: a screen
 * reader needs "loading files", not twelve grey rectangles.
 */
withDefaults(
  defineProps<{
    /** CSS length. Defaults to filling its container. */
    width?: string;
    height?: string;
    radius?: string;
    /** Repeats the block, for list placeholders. */
    lines?: number;
  }>(),
  { width: "100%", height: "var(--dj-type-body-size)", radius: "var(--dj-radius-sm)", lines: 1 },
);
</script>

<template>
  <div class="dj-skeleton-group" aria-hidden="true">
    <div
      v-for="line in lines"
      :key="line"
      class="dj-skeleton"
      :style="{
        width: line === lines && lines > 1 ? `calc(${width} * 0.6)` : width,
        height,
        borderRadius: radius,
      }"
    />
  </div>
</template>

<style scoped>
.dj-skeleton-group {
  display: flex;
  flex-direction: column;
  gap: var(--dj-space-2);
  width: 100%;
}
</style>
