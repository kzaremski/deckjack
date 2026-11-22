<script setup lang="ts">
import { computed } from "vue";

import { useShell } from "../shell/context.ts";
import { riskLabel } from "../stores/commands.ts";

/**
 * The confirmation for one of my own destructive commands.
 *
 * My actions run directly; destructive ones get an ordinary confirmation.
 * This is that, and it shows the count when the backend reported one —
 * "delete 214 messages" is a different decision from "delete messages", and
 * a count is the cheapest protection against a command that matched more
 * than it was meant to.
 */
const shell = useShell();
const pending = computed(() => shell.ui.confirmation.value);
</script>

<template>
  <div v-if="pending" class="scrim">
    <div class="dialog" role="alertdialog" aria-modal="true" :aria-label="pending.title">
      <span class="risk" :data-risk="pending.risk">{{ riskLabel(pending.risk) }}</span>
      <h2>{{ pending.title }}</h2>
      <p>{{ pending.detail }}</p>
      <!-- Text interpolation, never v-html: the target comes from params,
           and params can name anything. -->
      <p v-if="pending.target" class="target" data-testid="confirm-target">
        {{ pending.target }}
      </p>
      <p v-if="pending.count !== undefined" class="count">
        Affects <strong>{{ pending.count }}</strong> items.
      </p>
      <code>{{ pending.command }}</code>
      <div class="actions">
        <button type="button" class="ghost" @click="pending.resolve(false)">Cancel</button>
        <button type="button" class="go" autofocus @click="pending.resolve(true)">
          {{ pending.action ?? (pending.risk === "destructive" ? "Do it" : "Confirm") }}
        </button>
      </div>
    </div>
  </div>
</template>

<style scoped>
.scrim {
  position: fixed;
  inset: 0;
  z-index: var(--dj-layer-toast);
  display: grid;
  place-items: center;
  padding: var(--dj-space-4);
  background: var(--dj-color-scrim);
}

.dialog {
  display: flex;
  flex-direction: column;
  gap: var(--dj-space-3);
  width: min(440px, 100%);
  padding: var(--dj-space-5);
  background: var(--dj-color-overlay);
  border: var(--dj-stroke-regular) solid var(--dj-color-border-strong);
  border-radius: var(--dj-radius-xl);
  box-shadow: var(--dj-color-shadow-high);
}

h2 {
  font-size: var(--dj-type-heading-size);
  font-weight: var(--dj-type-heading-weight);
}

p {
  color: var(--dj-color-text-muted);
  font-size: var(--dj-type-small-size);
  line-height: var(--dj-type-small-line);
}

.target {
  padding: var(--dj-space-2) var(--dj-space-3);
  border-radius: var(--dj-radius-md);
  background: var(--dj-color-sunken);
  color: var(--dj-color-text);
  font-family: var(--dj-font-mono);
  overflow-wrap: anywhere;
  /* The node already strips bidi controls; isolating the box as well means
     a right-to-left name cannot reorder the text around it. */
  unicode-bidi: isolate;
}

.count strong {
  color: var(--dj-color-text);
}

code {
  color: var(--dj-color-text-dim);
  font-size: var(--dj-type-micro-size);
}

.risk {
  align-self: flex-start;
  padding: 2px var(--dj-space-2);
  border-radius: var(--dj-radius-pill);
  font-size: var(--dj-type-micro-size);
  text-transform: uppercase;
  letter-spacing: var(--dj-type-micro-tracking);
  color: var(--dj-color-risk-destructive);
  background: var(--dj-color-danger-soft);
}

.risk[data-risk="write"] {
  color: var(--dj-color-risk-write);
  background: var(--dj-color-info-soft);
}

.actions {
  display: flex;
  justify-content: flex-end;
  gap: var(--dj-space-2);
  margin-top: var(--dj-space-2);
}

.ghost,
.go {
  min-height: var(--dj-touch);
  padding-inline: var(--dj-space-4);
  border-radius: var(--dj-radius-md);
  font-weight: var(--dj-type-strong-weight);
}

.ghost {
  border: var(--dj-stroke-regular) solid var(--dj-color-border);
  background: transparent;
  color: var(--dj-color-text);
}

.go {
  border: 0;
  background: var(--dj-color-danger);
  color: var(--dj-color-text-on-danger);
}
</style>
