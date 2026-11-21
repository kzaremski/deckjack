<script lang="ts">
import { toRaw } from "vue";

import type { ShellWindow } from "../../stores/windows.ts";
import { foundation } from "../../tokens/foundation.ts";
import type { DensityName } from "../../tokens/types.ts";

/**
 * The newest launch each window's app has actually been handed, by URL or by
 * message. Kept outside any one panel because a window outlives its panel:
 * the compact shell reuses one panel for whichever window is focused, and a
 * change of layout remounts every panel. Keyed by the window object, so it
 * goes when the window does.
 */
const delivered = new WeakMap<object, number>();

function deliveredSeq(w: ShellWindow): number {
  return delivered.get(toRaw(w)) ?? 0;
}

function markDelivered(w: ShellWindow, seq: number) {
  if (seq > deliveredSeq(w)) delivered.set(toRaw(w), seq);
}

/** How long to keep asking a starting app before loading whatever it says. */
const MAX_PROBES = 120;

/** A 404 is retried this many times, this far apart, before it is believed:
 * long enough for the node to record a new window and start its app. */
const MAX_MISSING = 12;
const MISSING_RETRY_MS = 250;

/** `Retry-After` in milliseconds, kept between a quarter second and five. */
function retryAfter(answer: Response): number {
  const seconds = Number(answer.headers.get("retry-after"));
  const ms = Number.isFinite(seconds) && seconds > 0 ? seconds * 1000 : 1000;
  return Math.min(5000, Math.max(250, ms));
}

const FRAME_BASE_ID = "dj-frame-base";

/**
 * The part of the shell's base stylesheet that tokens depend on: a coarse
 * pointer raises every hit area to at least 44px whatever the density. A
 * frame does not load `styles/base.css`, so it gets the rule here.
 *
 * Each block repeats the density block's own selector, so it is exactly as
 * specific and wins by coming after the token stylesheet. A bare `:root`
 * would lose to `:root[data-density=...]`, media query or not. The values
 * are the density's literal ones rather than `var(--dj-row)`, which would
 * refer to itself and make the property invalid.
 */
const FRAME_BASE_CSS = `@media (pointer: coarse) {
${(Object.keys(foundation.density) as DensityName[])
  .map((name) => {
    const d = foundation.density[name];
    return `  :root[data-density="${name}"] {
    --dj-touch: max(44px, ${d.touch});
    --dj-row: max(44px, ${d.row});
  }`;
  })
  .join("\n")}
}`;
</script>

<script setup lang="ts">
import { onBeforeUnmount, ref, watch } from "vue";

import { useShell } from "../../shell/context.ts";
import { APP_LAUNCH_MESSAGE, frameSrc } from "../../shell/open-app.ts";
import { installTokenStylesheet } from "../../tokens/runtime.ts";
import type { AppLaunch } from "../../stores/windows.ts";

/**
 * An installed app's own web UI, in a window.
 *
 * The app's assets are served by the core at `/apps/<name>/`, on the core's
 * own origin. That is the point of routing them through the core rather than
 * letting the browser reach an app: the frame is same-origin, so the session
 * cookie applies, the app's UI can use the bus at `/ws` with no credentials
 * of its own, and there is no app address for the browser to hold.
 *
 * Same-origin is also what makes theming work. The shell reaches into the
 * frame's document and installs the same token stylesheet it uses itself, so
 * an app written against `var(--dj-color-surface)` follows the desktop's
 * theme without the app knowing a theme exists. It is re-installed whenever
 * the theme controller's revision changes, which is the counter that exists
 * for exactly this.
 *
 * A window opened on a file (`shell.app.open`) loads with `?path=`. A later
 * request to the same window is posted into the frame instead, because
 * changing `src` would reload the app and lose whatever it had not saved.
 * A launch is handed over once: a window shown again (the compact shell
 * switching back to it, or a change of layout) does not reopen the file it
 * was first opened on over whatever the operator did since.
 */
const props = defineProps<{ window: ShellWindow; compact?: boolean }>();

const shell = useShell();
const frame = ref<HTMLIFrameElement | null>(null);
const failed = ref(false);

/**
 * What the frame loads, or null while the app is not live yet.
 *
 * Not set until the node has recorded the window and the app answers its
 * page: an on-demand app is still spawning when its window first appears,
 * and the core answers `503` with `Retry-After` until it is up. A frame
 * pointed at that answer shows "restarting" for good, because nothing
 * reloads it, and a file posted into it is lost. So the panel asks first,
 * and loads the frame once the answer is the app.
 */
const src = ref<string | null>(null);
/** True while waiting for the app to come up, for the note under the frame. */
const starting = ref(false);

/** Stops this frame being able to ask the shell for anything. */
let detach: (() => void) | null = null;
/** Bumped whenever what this panel shows changes, so a stale wait stops. */
let generation = 0;
/** The loaded document is the app's, and can be posted to. */
let ready = false;
/** The launch the frame URL carries, recorded as delivered once it loads. */
let urlLaunch: AppLaunch | null = null;
/** Files asked for before the app's document loaded, posted on load. */
let queue: AppLaunch[] = [];
/**
 * The last file handed over by message, handed over again on every later
 * load. A reload goes back to the URL, which names the window's first file:
 * an app that restarts under an open window comes back through the core's
 * "restarting" page, which reloads itself until the app is up, and the app
 * would otherwise come up on the first file, not the one asked for last.
 */
let messaged: AppLaunch | null = null;
/**
 * The document `messaged` was posted into. A `load` for that same document
 * is not a reload, and handing the file over again would open it twice; a
 * document this panel cannot see (null) is treated as new.
 */
let messagedTo: Document | null = null;

function show(w: ShellWindow) {
  const token = ++generation;
  ready = false;
  queue = [];
  messaged = null;
  messagedTo = null;
  src.value = null;
  detach?.();
  detach = null;
  const launch = w.launch;
  urlLaunch = launch && launch.seq > deliveredSeq(w) ? { ...launch } : null;
  starting.value = true;
  void untilLive(w, token).then(() => {
    if (token !== generation) return;
    starting.value = false;
    src.value = frameSrc(w.appId, w.id, urlLaunch?.path);
  });
}

/**
 * Wait for the app to be up: ask for its page until the answer is the app.
 *
 * Two answers mean "not yet". `503` (with `Retry-After`) while the core is
 * starting or restarting the app. `404` for a moment at first: the window
 * is drawn before the node has recorded it, and until `shell.window.open`
 * lands the core has not started an on-demand app and does not serve its
 * page. A 404 that outlasts that is an app that cannot run, and the frame
 * shows it as it is.
 */
async function untilLive(w: ShellWindow, token: number): Promise<void> {
  const probe = frameSrc(w.appId, w.id);
  let missing = 0;
  for (let attempt = 0; attempt < MAX_PROBES; attempt++) {
    let answer: Response;
    try {
      answer = await fetch(probe, { cache: "no-store", credentials: "same-origin" });
    } catch {
      // The network itself failed. The frame will say so better than this
      // panel could, so load it.
      return;
    }
    void answer.body?.cancel().catch(() => {});
    if (token !== generation) return;
    let wait: number;
    if (answer.status === 503) wait = retryAfter(answer);
    else if (answer.status === 404 && ++missing <= MAX_MISSING) wait = MISSING_RETRY_MS;
    else return;
    await new Promise((resolve) => setTimeout(resolve, wait));
    if (token !== generation) return;
  }
}

function deliver(launch: AppLaunch) {
  const target = frame.value?.contentWindow;
  if (!ready || !target) {
    queue.push({ ...launch });
    return;
  }
  markDelivered(props.window, launch.seq);
  messaged = { ...launch };
  messagedTo = frame.value?.contentDocument ?? null;
  target.postMessage({ type: APP_LAUNCH_MESSAGE, path: launch.path }, window.location.origin);
}

show(props.window);

/**
 * A different window (the compact shell's one panel follows focus) is shown
 * afresh. A new launch for the same window is a message, never a new URL:
 * that would reload the app under the operator and lose whatever it had not
 * saved.
 */
watch(
  () => [props.window.id, props.window.launch?.seq] as const,
  ([id], [previousId]) => {
    if (id !== previousId) {
      show(props.window);
      return;
    }
    const launch = props.window.launch;
    if (!launch || launch.seq <= deliveredSeq(props.window)) return;
    if (launch.seq === urlLaunch?.seq || queue.some((q) => q.seq === launch.seq)) return;
    // Nothing loaded yet and nothing for the URL to carry: the URL takes it.
    if (src.value === null && !urlLaunch) {
      urlLaunch = { ...launch };
      return;
    }
    deliver(launch);
  },
);

/** On every load, because a navigation inside the frame is a new window. */
function loaded() {
  // A frame with nothing to load yet fires `load` for its blank document;
  // that is not the app, and must not be handed anything.
  if (src.value === null) return;
  detach?.();
  detach = shell.frames.attach(frame.value?.contentWindow ?? null);
  theme();
  ready = true;
  if (urlLaunch) markDelivered(props.window, urlLaunch.seq);
  urlLaunch = null;
  const waiting = queue;
  queue = [];
  const doc = frame.value?.contentDocument ?? null;
  if (waiting.length === 0 && messaged && (doc === null || doc !== messagedTo)) waiting.push(messaged);
  for (const launch of waiting) deliver(launch);
}

/**
 * Install (or re-install) the tokens inside the frame, with the attributes
 * they are keyed on. The token stylesheet scopes density and theme to
 * `:root[data-density]` and `:root[data-theme]`, so without the attributes
 * only the foundation matches: `--dj-touch` never resolves, every touch
 * target falls back to its 32px default, and the colors to the app's own.
 */
function theme() {
  const doc = frame.value?.contentDocument;
  if (!doc) return;
  try {
    const tokens = installTokenStylesheet(doc);
    const root = doc.documentElement;
    root.dataset.theme = shell.theme.theme.value;
    root.dataset.density = shell.theme.density.value;
    let base = doc.getElementById(FRAME_BASE_ID);
    if (!base) {
      base = doc.createElement("style");
      base.id = FRAME_BASE_ID;
      tokens.after(base);
    }
    base.textContent = FRAME_BASE_CSS;
  } catch {
    // A cross-origin frame would throw, and should: it would mean the core
    // started serving an app from somewhere else, which is the thing this
    // arrangement exists to prevent. Nothing to do here but leave the app
    // with its own fallback colors.
    failed.value = true;
  }
}

watch(() => shell.theme.revision.value, theme);

onBeforeUnmount(() => {
  // Nothing to tear down: the frame goes with the component, and the app
  // behind it is stopped by the core's idle sweep once its last window
  // closed. An app is deliberately not killed when a window closes — the
  // same reason a pty outlives its panel.
  generation++;
  detach?.();
  frame.value = null;
});
</script>

<template>
  <iframe
    ref="frame"
    class="frame"
    :src="src ?? undefined"
    :title="props.window.title"
    :data-compact="props.compact ? '' : undefined"
    @load="loaded"
  />
  <p v-if="starting" class="note" role="status">Starting {{ props.window.title }}…</p>
  <p v-if="failed" class="note">
    This app is served from another origin, so the desktop's theme was not
    applied.
  </p>
</template>

<style scoped>
.frame {
  display: block;
  flex: 1;
  width: 100%;
  min-height: 0;
  border: 0;
  background: var(--dj-color-surface);
}

.note {
  margin: 0;
  padding: var(--dj-space-2) var(--dj-space-3);
  background: var(--dj-color-warning-soft, var(--dj-color-surface-raised));
  color: var(--dj-color-text-muted);
  font-size: var(--dj-text-xs);
}
</style>
