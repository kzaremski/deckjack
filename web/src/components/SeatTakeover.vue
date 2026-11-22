<script setup lang="ts">
import { computed, nextTick, onMounted, ref } from "vue";

import { useShell } from "../shell/context.ts";
import { clock, DEVICE_MAX } from "../stores/seat.ts";

/**
 * What a device sees while another one is using Deckjack.
 *
 * The node keeps one workspace and lets one connection use it at a time, so
 * this replaces the workspace rather than covering it: nothing behind it
 * could be used anyway, and the windows and terminals now belong to the
 * device that has the seat. It says which device that is and since when,
 * and offers the one thing that can be done from here — take it over, which
 * asks first through the shell's own confirmation.
 *
 * Touch-first: one large button, text that wraps, nothing that needs a
 * pointer.
 */
const shell = useShell();
const seat = shell.seat;

const holder = computed(() => seat.status.value?.holder ?? null);
const taken = computed(() => seat.phase.value === "taken");

const heading = computed(() => {
  const who = holder.value?.device ?? "another device";
  return taken.value ? `Taken over by ${who}` : `Deckjack is in use on ${who}`;
});

const since = computed(() => (holder.value ? clock(holder.value.since_ms) : null));

/**
 * Focus moves to the heading when this screen appears. It replaces the
 * workspace — often under the operator's fingers, mid-keystroke in a
 * terminal — and whatever had focus is gone; without this, focus falls to
 * the body and a screen reader says nothing about why.
 */
const title = ref<HTMLElement | null>(null);
onMounted(() => title.value?.focus());

/**
 * The kill switch, reachable from here: the node runs `system.lockdown` for
 * any signed-in device whoever holds the seat, and the button for it in the
 * top bar is not on this screen.
 */
function lockDown() {
  void shell.commands.dispatch("system.lockdown");
}

const editing = ref(false);
const draft = ref("");
const field = ref<HTMLInputElement | null>(null);

async function startRename() {
  draft.value = seat.device.label.value;
  editing.value = true;
  await nextTick();
  field.value?.select();
}

async function saveRename() {
  editing.value = false;
  await seat.rename(draft.value);
}
</script>

<template>
  <main class="takeover" data-testid="seat-takeover" :data-phase="seat.phase.value">
    <div class="card">
      <h1 ref="title" tabindex="-1" aria-live="polite">{{ heading }}</h1>
      <p v-if="since" class="since">
        {{ taken ? "At" : "Since" }} {{ since }}<template v-if="holder && !holder.connected"> · reconnecting</template>
      </p>
      <p class="detail">
        The workspace is used on one device at a time. Its windows and terminals are on
        {{ holder?.device ?? "the other device" }} now; taking over moves them here.
      </p>

      <button
        type="button"
        class="take"
        :disabled="seat.busy.value"
        @click="seat.takeOver()"
      >
        {{ taken ? "Take back" : "Take over" }}
      </button>
      <p v-if="seat.lastError.value" class="why" role="status">{{ seat.lastError.value }}</p>

      <button type="button" class="ghost lock" @click="lockDown">Lock down this workspace</button>

      <div class="device">
        <template v-if="!editing">
          <span>This device: <strong data-testid="seat-device">{{ seat.device.label.value }}</strong></span>
          <button type="button" class="ghost" @click="startRename">Rename</button>
        </template>
        <form v-else class="rename" @submit.prevent="saveRename">
          <label for="dj-seat-device">Name this device</label>
          <input
            id="dj-seat-device"
            ref="field"
            v-model="draft"
            :maxlength="DEVICE_MAX"
            autocomplete="off"
            enterkeyhint="done"
          />
          <button type="submit" class="ghost">Save</button>
        </form>
      </div>
    </div>
  </main>
</template>

<style scoped>
.takeover {
  flex: 1;
  min-height: 0;
  display: grid;
  place-items: center;
  padding: var(--dj-space-4);
  padding-top: calc(var(--dj-space-4) + env(safe-area-inset-top, 0px));
  padding-bottom: calc(var(--dj-space-4) + env(safe-area-inset-bottom, 0px));
  overflow-y: auto;
}

.card {
  display: flex;
  flex-direction: column;
  gap: var(--dj-space-3);
  width: min(30rem, 100%);
  padding: var(--dj-space-5);
  border: var(--dj-stroke-regular) solid var(--dj-color-border);
  border-radius: var(--dj-radius-xl);
  background: var(--dj-color-raised);
}

h1 {
  margin: 0;
  outline: none;
  font-size: var(--dj-type-title-size);
  overflow-wrap: anywhere;
  /* A device label is text another device chose: keep it from reordering
     the sentence around it. */
  unicode-bidi: isolate;
}

.since {
  color: var(--dj-color-text);
  font-variant-numeric: tabular-nums;
}

.detail,
.why {
  color: var(--dj-color-text-muted);
  font-size: var(--dj-type-small-size);
  line-height: var(--dj-type-small-line);
}

.take {
  min-height: calc(var(--dj-touch) * 1.25);
  width: 100%;
  border: 0;
  border-radius: var(--dj-radius-md);
  background: var(--dj-color-accent);
  color: var(--dj-color-text-on-accent);
  font-size: var(--dj-type-heading-size);
  font-weight: var(--dj-type-strong-weight);
}

.take:disabled {
  opacity: 0.6;
}

.device {
  display: flex;
  flex-wrap: wrap;
  align-items: center;
  justify-content: space-between;
  gap: var(--dj-space-2);
  padding-top: var(--dj-space-3);
  border-top: var(--dj-stroke-regular) solid var(--dj-color-border);
  color: var(--dj-color-text-muted);
  font-size: var(--dj-type-small-size);
}

.device strong {
  color: var(--dj-color-text);
  unicode-bidi: isolate;
}

.rename {
  display: flex;
  flex-wrap: wrap;
  align-items: center;
  gap: var(--dj-space-2);
  width: 100%;
}

.rename label {
  width: 100%;
}

.rename input {
  flex: 1;
  min-width: 0;
  min-height: var(--dj-touch);
  padding-inline: var(--dj-space-3);
  border: var(--dj-stroke-regular) solid var(--dj-color-border-strong);
  border-radius: var(--dj-radius-md);
  background: var(--dj-color-sunken);
  color: var(--dj-color-text);
  font-size: 16px; /* below 16px, iOS zooms the page on focus */
}

.ghost {
  min-height: var(--dj-touch);
  padding-inline: var(--dj-space-4);
  border: var(--dj-stroke-regular) solid var(--dj-color-border);
  border-radius: var(--dj-radius-md);
  background: transparent;
  color: var(--dj-color-text);
}
</style>
