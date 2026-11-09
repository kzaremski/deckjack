<script setup lang="ts">
import { computed, onBeforeUnmount, onMounted, ref } from "vue";

import { useShell } from "../shell/context.ts";
import { apps, dockOrder } from "../stores/apps.ts";

/**
 * The dock: the launcher, what is running, and the state of the node.
 *
 * Every button dispatches a command rather than calling a store, so the dock
 * is a view over the command bus like the palette is. The Assistant's pending
 * approvals show here as a badge, which is the doc's rule that the Assistant
 * is one app among many and its Inbox surfaces rather than taking over.
 */
const shell = useShell();

const now = ref(new Date());
let timer = 0;

onMounted(() => {
  // A clock that ticks every second repaints the dock sixty times a minute
  // for a display that only changes once. Fifteen seconds is close enough to
  // right and cheap enough to leave running on a phone.
  timer = window.setInterval(() => {
    now.value = new Date();
  }, 15000);
});

onBeforeUnmount(() => window.clearInterval(timer));

const time = computed(() =>
  now.value.toLocaleTimeString([], { hour: "numeric", minute: "2-digit" }),
);
const date = computed(() =>
  now.value.toLocaleDateString([], { weekday: "short", month: "short", day: "numeric" }),
);

const nodeLabel = computed(() => {
  const node = shell.bus.node.value;
  if (node) return `${node.name} · gen ${node.generation}`;
  switch (shell.bus.status.value) {
    case "open":
      return "connected";
    case "connecting":
      return "connecting";
    case "closed":
      return "disconnected";
    default:
      return "no bus";
  }
});

const items = computed(() =>
  dockOrder.map((id) => {
    const open = shell.windows.all.value.filter((w) => w.app === id);
    const active = open.some(
      (w) => !w.minimized && w.id === shell.windows.focusedId.value,
    );
    return {
      app: apps[id],
      running: open.length > 0,
      active,
      badge: apps[id].badge === "inbox" ? shell.inboxPending.value : 0,
      windowId: open[0]?.id ?? null,
    };
  }),
);

function activate(id: string, windowId: string | null, active: boolean) {
  if (!windowId) {
    void shell.commands.dispatch(`app.open.${id}`);
    return;
  }
  if (active) {
    shell.windows.minimize(windowId);
    return;
  }
  shell.windows.focus(windowId);
}
</script>

<template>
  <nav class="dock" aria-label="Dock">
    <button
      type="button"
      class="brand"
      @click="shell.commands.dispatch('shell.palette.open')"
    >
      <span class="mark" aria-hidden="true">dj</span>
      <span class="word">deckjack</span>
      <kbd v-if="shell.keyboard.labelFor('shell.palette.open')">
        {{ shell.keyboard.labelFor("shell.palette.open") }}
      </kbd>
    </button>

    <span class="rule" aria-hidden="true" />

    <div class="items">
      <button
        v-for="item in items"
        :key="item.app.id"
        type="button"
        class="item"
        :class="{ running: item.running, active: item.active }"
        :title="item.app.summary"
        :aria-pressed="item.active"
        @click="activate(item.app.id, item.windowId, item.active)"
      >
        <span
          class="tag"
          :data-accent="item.app.id === 'assistant' ? '' : undefined"
          aria-hidden="true"
          >{{ item.app.tag }}</span
        >
        <span class="label">{{ item.app.title }}</span>
        <span v-if="item.badge > 0" class="badge">
          {{ item.badge }}
          <span class="dj-sr">approvals waiting</span>
        </span>
        <span class="dot" aria-hidden="true" />
      </button>
    </div>

    <span class="spacer" />

    <span class="node" :data-status="shell.bus.status.value">
      <span class="pip" aria-hidden="true" />
      {{ nodeLabel }}
    </span>

    <button
      type="button"
      class="icon"
      aria-label="Lock down this workspace"
      @click="shell.commands.dispatch('system.lockdown')"
    >
      <svg viewBox="0 0 24 24" aria-hidden="true">
        <rect x="5" y="11" width="14" height="10" rx="2" />
        <path d="M8 11V8a4 4 0 0 1 8 0v3" />
      </svg>
    </button>

    <div class="clock">
      <span>{{ time }}</span>
      <span class="muted">{{ date }}</span>
    </div>
  </nav>
</template>

<style scoped>
.dock {
  display: flex;
  align-items: center;
  gap: var(--dj-space-1);
  flex: none;
  padding-inline: var(--dj-space-3);
  padding-block: var(--dj-space-1);
  /* Respects a phone's home-indicator inset; `viewport-fit=cover` is set. */
  padding-bottom: calc(var(--dj-space-1) + env(safe-area-inset-bottom, 0px));
  background: var(--dj-color-raised);
  border-top: var(--dj-stroke-regular) solid var(--dj-color-border);
  z-index: var(--dj-layer-dock);
}

.brand,
.item,
.icon {
  display: flex;
  align-items: center;
  gap: var(--dj-space-2);
  min-height: var(--dj-touch);
  padding-inline: var(--dj-space-2);
  border: 0;
  border-radius: var(--dj-radius-md);
  background: transparent;
  color: var(--dj-color-text);
  font-size: var(--dj-type-body-size);
}

.brand {
  font-weight: var(--dj-type-strong-weight);
}

.brand kbd {
  font-family: var(--dj-font-mono);
  font-size: var(--dj-type-micro-size);
  color: var(--dj-color-text-dim);
}

.mark {
  display: grid;
  place-items: center;
  width: 26px;
  height: 26px;
  border-radius: var(--dj-radius-sm);
  background: var(--dj-color-accent);
  color: var(--dj-color-text-on-accent);
  font-family: var(--dj-font-mono);
  font-size: var(--dj-type-micro-size);
}

.rule {
  width: var(--dj-stroke-regular);
  height: 24px;
  margin-inline: var(--dj-space-1);
  background: var(--dj-color-border);
}

.items {
  display: flex;
  align-items: center;
  gap: var(--dj-space-1);
  min-width: 0;
  overflow-x: auto;
  scrollbar-width: none;
}

.item {
  position: relative;
  flex: none;
}

.item .label {
  color: var(--dj-color-text-muted);
}

.item.running .label,
.item.active .label {
  color: var(--dj-color-text);
}

.item.active {
  background: var(--dj-color-sunken);
}

.tag {
  display: grid;
  place-items: center;
  width: 24px;
  height: 24px;
  border-radius: var(--dj-radius-sm);
  background: var(--dj-color-sunken);
  color: var(--dj-color-text-muted);
  font-family: var(--dj-font-mono);
  font-size: var(--dj-type-micro-size);
}

.tag[data-accent] {
  background: var(--dj-color-accent);
  color: var(--dj-color-text-on-accent);
}

.badge {
  display: grid;
  place-items: center;
  min-width: 20px;
  height: 20px;
  padding-inline: var(--dj-space-1);
  border-radius: var(--dj-radius-pill);
  background: var(--dj-color-warn);
  color: var(--dj-color-text-on-warn);
  font-size: var(--dj-type-micro-size);
  font-weight: 700;
}

/* The running indicator is a bar under the item, not a change of color:
   color alone would be the only cue, and two of the themes' accents are
   close in lightness to the muted text. */
.dot {
  position: absolute;
  inset-block-end: 2px;
  inset-inline-start: 50%;
  width: 6px;
  height: 3px;
  margin-inline-start: -3px;
  border-radius: var(--dj-radius-pill);
  background: transparent;
  transition: width var(--dj-duration-fast) var(--dj-ease-standard);
}

.item.running .dot {
  background: var(--dj-color-text-dim);
}

.item.active .dot {
  width: 18px;
  margin-inline-start: -9px;
  background: var(--dj-color-accent);
}

.spacer {
  flex: 1;
}

.node {
  display: flex;
  align-items: center;
  gap: var(--dj-space-2);
  flex: none;
  height: var(--dj-control);
  padding-inline: var(--dj-space-3);
  border: var(--dj-stroke-regular) solid var(--dj-color-border);
  border-radius: var(--dj-radius-pill);
  font-family: var(--dj-font-mono);
  font-size: var(--dj-type-micro-size);
  color: var(--dj-color-text-muted);
  white-space: nowrap;
}

.pip {
  width: 8px;
  height: 8px;
  border-radius: var(--dj-radius-pill);
  background: var(--dj-color-text-dim);
}

.node[data-status="open"] .pip {
  background: var(--dj-color-ok);
}

.node[data-status="connecting"] .pip {
  background: var(--dj-color-warn);
}

.node[data-status="detached"] .pip,
.node[data-status="closed"] .pip {
  background: var(--dj-color-danger);
}

.icon {
  justify-content: center;
  width: var(--dj-touch);
  color: var(--dj-color-text-muted);
}

.icon svg {
  width: 18px;
  height: 18px;
  fill: none;
  stroke: currentColor;
  stroke-width: 2;
  stroke-linecap: round;
  stroke-linejoin: round;
}

.clock {
  display: flex;
  flex-direction: column;
  align-items: flex-end;
  flex: none;
  padding-inline: var(--dj-space-2);
  font-size: var(--dj-type-small-size);
  line-height: 1.2;
}

.clock .muted {
  color: var(--dj-color-text-muted);
}

@media (hover: hover) {
  .brand:hover,
  .item:hover,
  .icon:hover {
    background: var(--dj-color-sunken);
  }
}

/* Below a tablet the dock keeps only what is useful at that width: the
   launcher, the running apps and the clock. */
@media (max-width: 839px) {
  .brand .word,
  .item .label,
  .node {
    display: none;
  }
}
</style>
