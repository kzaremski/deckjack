<script setup lang="ts">
import { computed, onBeforeUnmount, onMounted, ref } from "vue";

import { useShell } from "../shell/context.ts";

/**
 * What the shell says when it is not connected to its node.
 *
 * It is a card and not a notice on purpose. A notice is for something that
 * happened; this is a *state*, and it has to stay on screen for as long as it
 * is true. The whole point of the shell is that the node holds the work — so
 * while there is no node there is nothing here to use, and saying so plainly
 * beats letting someone click a dead launcher and guess why nothing happened.
 *
 * It names which of the two failures it is. "This device is offline" is the
 * operator's wifi and they can fix it; "the node is not answering" with a
 * network present means the machine their work lives on is down, which is a
 * different errand entirely. `navigator.onLine` is only trusted when it says
 * false — a captive portal reports true — so it explains a failure the bus
 * already detected rather than deciding one happened.
 */
const shell = useShell();

const now = ref(Date.now());
let timer = 0;

onMounted(() => {
  // One second, because this card counts down to the next attempt and shows
  // how long the node has been silent. Both are only legible if they move.
  timer = window.setInterval(() => {
    now.value = Date.now();
  }, 1000);
});

onBeforeUnmount(() => window.clearInterval(timer));

const status = computed(() => shell.bus.status.value);
const offline = computed(() => !shell.bus.online.value);
const show = computed(() => status.value !== "open");

/** "closed" is the one disconnected state nothing is retrying. */
const stopped = computed(() => status.value === "closed");

const heading = computed(() => {
  if (stopped.value) return "Disconnected";
  if (offline.value) return "This device is offline";
  return status.value === "connecting" ? "Reconnecting" : "Node not answering";
});

const nodeName = computed(() => shell.bus.node.value?.name ?? null);

const detail = computed(() => {
  if (offline.value) {
    return "The shell is waiting for a network. Nothing can change until this device is back online — your windows and files are on the node, not here.";
  }
  if (stopped.value) {
    return "The connection was closed and nothing is retrying it.";
  }
  const who = nodeName.value ? `${nodeName.value}` : "the shell node";
  return `The shell cannot reach ${who}. Your windows, terminals and files live there, so nothing can be opened or changed until it answers.`;
});

/** Seconds of silence, once it is long enough to be worth saying. */
const silentFor = computed(() => {
  const at = shell.bus.lastSeenAt.value;
  if (at === null) return null;
  const seconds = Math.floor((now.value - at) / 1000);
  return seconds >= 5 ? seconds : null;
});

const retryIn = computed(() => {
  const at = shell.bus.retryAt.value;
  if (at === null || stopped.value) return null;
  return Math.max(0, Math.ceil((at - now.value) / 1000));
});
</script>

<template>
  <!--
    `aria-live="assertive"` and `role="alert"`: losing the node is not a
    decoration, and someone using a screen reader has to be told that the
    thing they are about to operate is not there.
  -->
  <div v-if="show" class="scrim" role="alert" aria-live="assertive">
    <div class="card" :data-state="offline ? 'offline' : status">
      <div class="head">
        <span class="pip" aria-hidden="true" />
        <h2>{{ heading }}</h2>
      </div>

      <p class="detail">{{ detail }}</p>

      <dl class="facts">
        <div v-if="nodeName">
          <dt>Node</dt>
          <dd>{{ nodeName }}</dd>
        </div>
        <div v-if="silentFor !== null">
          <dt>Silent for</dt>
          <dd>{{ silentFor }}s</dd>
        </div>
        <div v-if="retryIn !== null">
          <dt>Next try</dt>
          <dd>{{ retryIn === 0 ? "now" : `${retryIn}s` }}</dd>
        </div>
      </dl>

      <p v-if="shell.bus.lastError.value" class="why">{{ shell.bus.lastError.value }}</p>

      <button type="button" class="retry" @click="shell.bus.reconnectNow()">
        Try now
      </button>
    </div>
  </div>
</template>

<style scoped>
/*
 * It covers the shell and takes the pointer. That is the point: the windows
 * behind it cannot be used, so letting them be clicked would be the same lie
 * in a quieter form. It is translucent rather than opaque so the workspace
 * stays visible — what is open is still true, it just cannot be changed.
 */
.scrim {
  position: fixed;
  inset: 0;
  z-index: var(--dj-layer-modal, 90);
  display: grid;
  place-items: center;
  padding: var(--dj-space-4);
  padding-top: calc(var(--dj-space-4) + env(safe-area-inset-top, 0px));
  background: color-mix(in srgb, var(--dj-color-ground) 72%, transparent);
  backdrop-filter: blur(2px);
}

.card {
  width: min(28rem, 100%);
  padding: var(--dj-space-5);
  border: var(--dj-stroke-regular) solid var(--dj-color-border);
  border-radius: var(--dj-radius-lg);
  background: var(--dj-color-raised);
  box-shadow: var(--dj-shadow-raised, 0 8px 24px rgba(0, 0, 0, 0.35));
}

.head {
  display: flex;
  align-items: center;
  gap: var(--dj-space-2);
  margin-bottom: var(--dj-space-3);
}

h2 {
  margin: 0;
  font-size: var(--dj-type-title-size);
}

/* Amber while something is still trying, red once nothing is. Paired with
   the heading text, never color alone. */
.pip {
  width: 10px;
  height: 10px;
  flex: none;
  border-radius: var(--dj-radius-pill);
  background: var(--dj-color-warn);
}

.card[data-state="closed"] .pip,
.card[data-state="offline"] .pip {
  background: var(--dj-color-danger);
}

.card[data-state="connecting"] .pip {
  animation: pulse 1.4s var(--dj-ease-standard) infinite;
}

@keyframes pulse {
  50% {
    opacity: 0.25;
  }
}

@media (prefers-reduced-motion: reduce) {
  .card[data-state="connecting"] .pip {
    animation: none;
  }
}

.detail {
  margin: 0 0 var(--dj-space-4);
  color: var(--dj-color-text-muted);
}

.facts {
  display: flex;
  flex-wrap: wrap;
  gap: var(--dj-space-4);
  margin: 0 0 var(--dj-space-4);
}

.facts div {
  display: flex;
  flex-direction: column;
  gap: 2px;
}

dt {
  font-size: var(--dj-type-micro-size);
  text-transform: uppercase;
  letter-spacing: 0.06em;
  color: var(--dj-color-text-dim);
}

dd {
  margin: 0;
  font-family: var(--dj-font-mono);
  font-size: var(--dj-type-small-size);
}

.why {
  margin: 0 0 var(--dj-space-4);
  padding: var(--dj-space-2) var(--dj-space-3);
  border-radius: var(--dj-radius-sm);
  background: var(--dj-color-sunken);
  font-family: var(--dj-font-mono);
  font-size: var(--dj-type-micro-size);
  color: var(--dj-color-text-muted);
}

.retry {
  width: 100%;
  min-height: var(--dj-touch);
  border: 0;
  border-radius: var(--dj-radius-md);
  background: var(--dj-color-accent);
  color: var(--dj-color-text-on-accent);
  font: inherit;
  font-weight: var(--dj-type-strong-weight);
}
</style>
