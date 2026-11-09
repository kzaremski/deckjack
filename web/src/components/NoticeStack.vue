<script setup lang="ts">
import { useShell } from "../shell/context.ts";

/**
 * Notices: what went wrong, and what is waiting on an approval.
 *
 * Informational ones time out; errors stay until dismissed, because an error
 * that vanishes on its own is an error nobody read. They sit above the dock
 * and never move anything: the stack is positioned, so content does not
 * shift when one arrives.
 */
const shell = useShell();
</script>

<template>
  <div class="stack" role="status" aria-live="polite">
    <div
      v-for="notice in shell.ui.notices.value"
      :key="notice.id"
      class="notice"
      :data-kind="notice.kind"
    >
      <span class="text">
        <code v-if="notice.command">{{ notice.command }}</code>
        {{ notice.text }}
      </span>
      <button type="button" aria-label="Dismiss" @click="shell.ui.dismiss(notice.id)">×</button>
    </div>
  </div>
</template>

<style scoped>
.stack {
  position: fixed;
  inset-block-end: calc(var(--dj-touch) + var(--dj-space-4));
  inset-inline-end: var(--dj-space-4);
  z-index: var(--dj-layer-toast);
  display: flex;
  flex-direction: column;
  gap: var(--dj-space-2);
  width: min(380px, calc(100vw - var(--dj-space-8)));
  pointer-events: none;
}

.notice {
  display: flex;
  align-items: flex-start;
  gap: var(--dj-space-3);
  padding: var(--dj-space-3);
  background: var(--dj-color-overlay);
  border: var(--dj-stroke-regular) solid var(--dj-color-border);
  border-radius: var(--dj-radius-md);
  box-shadow: var(--dj-color-shadow-mid);
  font-size: var(--dj-type-small-size);
  pointer-events: auto;
  animation: in var(--dj-duration-base) var(--dj-ease-entrance);
}

@keyframes in {
  from {
    transform: translateY(6px);
    opacity: 0;
  }
}

.notice[data-kind="warn"] {
  border-color: var(--dj-color-warn);
}

.notice[data-kind="error"] {
  border-color: var(--dj-color-danger);
}

.text {
  flex: 1;
  min-width: 0;
}

code {
  display: block;
  color: var(--dj-color-text-dim);
  font-size: var(--dj-type-micro-size);
}

button {
  flex: none;
  width: 24px;
  height: 24px;
  border: 0;
  border-radius: var(--dj-radius-sm);
  background: transparent;
  color: var(--dj-color-text-muted);
  font-size: 16px;
  line-height: 1;
}
</style>
