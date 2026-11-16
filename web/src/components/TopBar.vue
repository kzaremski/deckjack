<script setup lang="ts">
import { computed, onBeforeUnmount, onMounted, ref } from "vue";

import { useShell } from "../shell/context.ts";
import { apps, dockOrder } from "../stores/apps.ts";

/**
 * The bar: the launcher, what is running, the layout, and the state of the node.
 *
 * At the top, like i3bar or waybar, and one bar for both device classes.
 * Compact used to carry a second header of its own, which on a phone spent two
 * rows of a short screen saying where you were; the window title and the
 * switcher moved here instead.
 *
 * Every button dispatches a command rather than calling a store, so the bar is
 * a view over the command bus like the palette is. The Assistant's pending
 * approvals show here as a badge, which is the doc's rule that the Assistant is
 * one app among many and its Inbox surfaces rather than taking over.
 */
const shell = useShell();

const now = ref(new Date());
let timer = 0;

onMounted(() => {
  // A clock that ticks every second repaints the bar sixty times a minute for
  // a display that only changes once. Fifteen seconds is close enough to right
  // and cheap enough to leave running on a phone.
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

const compact = computed(() => shell.viewport.mode.value !== "desktop");
const tiling = computed(() => shell.windows.layout.value === "tiling");

/** Compact only: which window the bar is standing in for. */
const current = computed(
  () => shell.windows.focused.value ?? shell.windows.recent.value[0] ?? null,
);
const currentTag = computed(() =>
  current.value ? shell.windows.contract(current.value).tag : null,
);
const split = computed(() => shell.windows.split.value);

/**
 * What the node pill says.
 *
 * The connection state wins over the node's name whenever they disagree. The
 * old version showed `name · gen N` as soon as a handshake had ever happened
 * and kept showing it after the connection dropped, so the one surface that
 * is always on screen claimed a live node while the bus knew otherwise.
 */
const nodeLabel = computed(() => {
  const node = shell.bus.node.value;
  const name = node ? node.name : "node";
  if (!shell.bus.online.value) return "offline";
  switch (shell.bus.status.value) {
    case "open":
      return node ? `${node.name} · gen ${node.generation}` : "connected";
    case "connecting":
      return `${name} · reconnecting`;
    case "closed":
      return `${name} · disconnected`;
    default:
      return `${name} · unreachable`;
  }
});

/**
 * Round-trip time, once measured. Proof the pill is not just optimistic.
 *
 * Gated on `online` as well as on the status, and for the same reason
 * `nodeLabel` is. Losing the network does not change the bus status -- the
 * socket is only condemned later, by the heartbeat, which is what finally
 * nulls `rtt` -- so for that whole window this read "offline 40ms": the name
 * correctly withdrawn, and a stale figure presented beside it as if it were
 * current. That is the same cached-value lie `nodeLabel` was written to
 * prevent, in the one place the fix was missed.
 */
const latency = computed(() => {
  const ms = shell.bus.rtt.value;
  if (!shell.bus.online.value) return null;
  return shell.bus.status.value === "open" && ms !== null ? `${ms}ms` : null;
});

const items = computed(() =>
  dockOrder.map((id) => {
    const open = shell.windows.all.value.filter((w) => w.app === id);
    const active = open.some((w) => !w.minimized && w.id === shell.windows.focusedId.value);
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
  <nav class="bar" aria-label="Taskbar">
    <button type="button" class="brand" @click="shell.commands.dispatch('shell.palette.open')">
      <span class="mark" aria-hidden="true">dj</span>
      <span class="word">deckjack</span>
      <kbd v-if="!compact && shell.keyboard.labelFor('shell.palette.open')">
        {{ shell.keyboard.labelFor("shell.palette.open") }}
      </kbd>
    </button>

    <span class="rule" aria-hidden="true" />

    <!--
      Compact: the bar names the window you are in and opens the switcher,
      which is the thing reached constantly on a phone. On the desktop the
      title lives in each window's own frame, so this would be noise.
    -->
    <button
      v-if="compact"
      type="button"
      class="switch"
      aria-label="Switch window"
      @click="shell.commands.dispatch('shell.switcher.open')"
    >
      <span v-if="currentTag" class="tag" aria-hidden="true">{{ currentTag }}</span>
      <span class="title">{{ current?.title ?? "Deckjack" }}</span>
      <svg viewBox="0 0 24 24" aria-hidden="true" class="chev">
        <path d="m7 10 5 5 5-5" />
      </svg>
    </button>

    <div v-else class="items">
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

    <!--
      Desktop: floating or tiling. Pressed means tiling, because floating is
      the default and an unpressed toggle should read as "nothing imposed".
    -->
    <button
      v-if="!compact"
      type="button"
      class="icon"
      :aria-pressed="tiling"
      :title="tiling ? 'Tiling: master and stack' : 'Floating windows'"
      :aria-label="tiling ? 'Switch to floating windows' : 'Switch to tiling layout'"
      @click="shell.commands.dispatch('shell.layout.toggle')"
    >
      <svg viewBox="0 0 24 24" aria-hidden="true">
        <template v-if="tiling">
          <rect x="3" y="4" width="8" height="16" rx="1" />
          <rect x="13" y="4" width="8" height="7" rx="1" />
          <rect x="13" y="13" width="8" height="7" rx="1" />
        </template>
        <template v-else>
          <rect x="3" y="5" width="12" height="10" rx="2" />
          <rect x="9" y="9" width="12" height="10" rx="2" />
        </template>
      </svg>
    </button>

    <!-- Compact: the split is that mode's answer to tiling. -->
    <button
      v-if="compact && shell.windows.recent.value.length >= 2"
      type="button"
      class="icon"
      :aria-pressed="split !== null"
      :aria-label="split ? 'Leave the split' : 'Split with the last window'"
      @click="shell.commands.dispatch('shell.split.toggle')"
    >
      <svg viewBox="0 0 24 24" aria-hidden="true">
        <rect x="3" y="4" width="18" height="16" rx="2" />
        <path d="M12 4v16" />
      </svg>
    </button>

    <button
      v-if="compact"
      type="button"
      class="icon"
      aria-label="Command palette"
      @click="shell.commands.dispatch('shell.palette.open')"
    >
      <svg viewBox="0 0 24 24" aria-hidden="true">
        <circle cx="11" cy="11" r="7" />
        <path d="m20 20-3.5-3.5" />
      </svg>
    </button>

    <span
      class="node"
      :data-status="shell.bus.online.value ? shell.bus.status.value : 'closed'"
    >
      <span class="pip" aria-hidden="true" />
      {{ nodeLabel }}
      <span v-if="latency" class="rtt">{{ latency }}</span>
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
.bar {
  display: flex;
  align-items: center;
  gap: var(--dj-space-1);
  flex: none;
  padding-inline: var(--dj-space-3);
  padding-block: var(--dj-space-1);
  /* At the top now, so it is the status-bar inset that matters, not the home
     indicator. `viewport-fit=cover` is set. */
  padding-top: calc(var(--dj-space-1) + env(safe-area-inset-top, 0px));
  background: var(--dj-color-raised);
  border-bottom: var(--dj-stroke-regular) solid var(--dj-color-border);
  z-index: var(--dj-layer-dock);
  /* Chrome, not content: a long press on a launcher should not offer to copy
     its label. Deliberately scoped here rather than set on `body`, where it
     would also kill long-press-to-select over terminal output. */
  -webkit-touch-callout: none;
  user-select: none;
}

.brand,
.item,
.icon,
.switch {
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

/* Compact's window button takes the slack so the title gets the room. */
.switch {
  flex: 1;
  min-width: 0;
  text-align: start;
}

.title {
  flex: 1;
  min-width: 0;
  overflow: hidden;
  white-space: nowrap;
  text-overflow: ellipsis;
  font-size: var(--dj-type-title-size);
  font-weight: var(--dj-type-strong-weight);
}

.chev {
  width: 18px;
  height: 18px;
  flex: none;
  fill: none;
  stroke: var(--dj-color-text-muted);
  stroke-width: 2;
  stroke-linecap: round;
}

.tag {
  display: grid;
  place-items: center;
  flex: none;
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

/* The running indicator is a bar under the item, not a change of color: color
   alone would be the only cue, and two of the themes' accents are close in
   lightness to the muted text. */
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

.rtt {
  color: var(--dj-color-text-dim);
}

/* The pill is the one status surface that survives at phone width, so it
   stops being hidden there the moment it has something wrong to report. */
@media (max-width: 839px) {
  .node[data-status="connecting"],
  .node[data-status="detached"],
  .node[data-status="closed"] {
    display: flex;
  }
}

.icon {
  justify-content: center;
  width: var(--dj-touch);
  flex: none;
  color: var(--dj-color-text-muted);
}

.icon[aria-pressed="true"] {
  background: var(--dj-color-accent-soft);
  color: var(--dj-color-accent);
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
  .icon:hover,
  .switch:hover {
    background: var(--dj-color-sunken);
  }
}

/* Below a tablet the bar keeps only what is useful at that width: the
   launcher, what is running or focused, and the clock. */
@media (max-width: 839px) {
  .brand .word,
  .item .label,
  .node {
    display: none;
  }

  .rule {
    display: none;
  }

  .clock .muted {
    display: none;
  }
}
</style>
