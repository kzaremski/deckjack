import { computed, ref, watch, type ComputedRef, type Ref } from "vue";

import type { SeatStatus } from "../protocol.ts";
import type { Bus } from "./bus.ts";
import type { UiStore } from "./ui.ts";
import type { WindowsStore } from "./windows.ts";

/**
 * The single seat, as this connection sees it.
 *
 * The node keeps one workspace, and only one connection uses it at a time:
 * the one that holds the seat (`crates/deckjack-core/src/seat.rs`). Every
 * other signed-in device sees who has it and can take it over. The node
 * enforces this — a call from a connection without the seat is refused with
 * `seat_not_held` — so this store only follows what the node says and asks
 * for the seat; it never decides that this device may act.
 *
 * What it adds on this side is the difference between two screens the node
 * does not distinguish: "in use on another device" (this connection never
 * had it) and "taken over by another device" (it had it, and lost it).
 */

/** Where the operator's name for this device is kept. Per device, by design. */
export const DEVICE_KEY = "deckjack.device";

/** The node keeps this many characters of a label; the field stops there too. */
export const DEVICE_MAX = 40;

export type SeatPhase =
  /** No word from the node yet on this connection. */
  | "unknown"
  /** This connection holds the seat: the workspace is ours. */
  | "held"
  /** Free for this connection, and being claimed without asking. */
  | "claiming"
  /** Another device has it; this connection never did. */
  | "in-use"
  /** This connection had it and another device took it. */
  | "taken";

export interface DeviceStorage {
  read(key: string): string | null;
  write(key: string, value: string): void;
}

/**
 * What the browser says about the device beyond its user agent string.
 *
 * Chrome sends a reduced user agent on Android ("Linux; Android 10; K"), so
 * the model is only in User-Agent Client Hints, which arrive asynchronously
 * (`navigator.userAgentData.getHighEntropyValues(["model"])`). iPadOS Safari
 * reports a Mac by default; a "Mac" with a touch screen is an iPad.
 */
export interface DeviceHints {
  /** The hardware model from client hints ("Pixel 8"), when known. */
  model?: string;
  /** `navigator.maxTouchPoints`. */
  touchPoints?: number;
}

/**
 * A readable default name for this device from its user agent: "Chrome on
 * Pixel", "Firefox on Linux", "Safari on iPhone". Only ever a default the
 * operator can change, so it errs short rather than exact.
 */
export function defaultDeviceLabel(userAgent: string, hints: DeviceHints = {}): string {
  const ua = userAgent;
  const model = (hints.model ?? "").trim();
  const touchMac = /Macintosh/.test(ua) && (hints.touchPoints ?? 0) > 1;
  const browser = /Edg(e|A|iOS)?\//.test(ua)
    ? "Edge"
    : /SamsungBrowser\//.test(ua)
      ? "Samsung Internet"
      : /OPR\/|Opera/.test(ua)
        ? "Opera"
        : /Firefox\/|FxiOS\//.test(ua)
          ? "Firefox"
          : /Chrome\/|CriOS\/|Chromium\//.test(ua)
            ? "Chrome"
            : /Safari\//.test(ua)
              ? "Safari"
              : "Browser";
  const platform = model
    ? model
    : /Pixel/.test(ua)
    ? "Pixel"
    : /iPhone/.test(ua)
      ? "iPhone"
      : /iPad/.test(ua) || touchMac
        ? "iPad"
        : /Android/.test(ua)
          ? "Android"
          : /CrOS/.test(ua)
            ? "ChromeOS"
            : /Macintosh|Mac OS X/.test(ua)
              ? "Mac"
              : /Windows/.test(ua)
                ? "Windows"
                : /Linux|X11/.test(ua)
                  ? "Linux"
                  : "this device";
  return `${browser} on ${platform}`;
}

/** What a label becomes before it is kept: trimmed, single-spaced, capped. */
export function cleanLabel(raw: string): string {
  const flat = raw.replace(/[\u0000-\u001f\u007f-\u009f​-‏‪-‮⁠-⁩﻿]/g, "");
  return flat.replace(/\s+/g, " ").trim().slice(0, DEVICE_MAX).trim();
}

/** The operator's name for this device, with its default. */
export interface DeviceLabel {
  label: Ref<string>;
  /** Keep a new name. An empty one goes back to the default. */
  rename(label: string): void;
  /**
   * Better hints arrived (the model, from client hints): recompute the
   * default, and show it unless the operator named this device.
   */
  refine(hints: DeviceHints): void;
}

export function createDeviceLabel(
  storage: DeviceStorage,
  userAgent: string,
  hints: DeviceHints = {},
): DeviceLabel {
  let known = hints;
  let fallback = cleanLabel(defaultDeviceLabel(userAgent, known));
  const named = () => cleanLabel(storage.read(DEVICE_KEY) ?? "");
  const label = ref(named() || fallback);
  return {
    label,
    rename(next) {
      const clean = cleanLabel(next);
      label.value = clean || fallback;
      storage.write(DEVICE_KEY, clean);
    },
    refine(more) {
      known = { ...known, ...more };
      fallback = cleanLabel(defaultDeviceLabel(userAgent, known));
      if (!named()) label.value = fallback;
    },
  };
}

/**
 * Ask the browser for the device model (User-Agent Client Hints), and refine
 * the label's default with it. A browser without client hints, or one that
 * declines, leaves the default as the user agent gave it.
 */
export function refineFromClientHints(device: DeviceLabel, nav: Navigator): void {
  const data = (nav as Navigator & {
    userAgentData?: { getHighEntropyValues?(hints: string[]): Promise<{ model?: string }> };
  }).userAgentData;
  if (!data?.getHighEntropyValues) return;
  data
    .getHighEntropyValues(["model"])
    .then((values) => {
      if (values.model) device.refine({ model: values.model });
    })
    .catch(() => {});
}

/** `HH:MM` in local time: when the holder took the seat, for the screen. */
export function clock(ms: number): string {
  const at = new Date(ms);
  const pad = (n: number) => String(n).padStart(2, "0");
  return `${pad(at.getHours())}:${pad(at.getMinutes())}`;
}

export interface SeatStore {
  status: Ref<SeatStatus | null>;
  phase: ComputedRef<SeatPhase>;
  /** True while this connection holds the seat. */
  held: ComputedRef<boolean>;
  /**
   * Whether to show the workspace at all. Everything but "another device has
   * it": before the node has said, and while a free seat is being claimed,
   * the workspace stays rather than flashing the takeover screen.
   */
  showWorkspace: ComputedRef<boolean>;
  device: DeviceLabel;
  /** A take is in flight. */
  busy: Ref<boolean>;
  lastError: Ref<string | null>;
  /**
   * Ask the operator, then take the seat. Resolves true when this connection
   * holds it afterwards.
   */
  takeOver(): Promise<boolean>;
  /** Rename this device, and tell the others if this device is seated. */
  rename(label: string): Promise<void>;
  dispose(): void;
}

export function createSeat(deps: { bus: Bus; ui: UiStore; device: DeviceLabel }): SeatStore {
  const { bus, ui, device } = deps;
  const status = ref<SeatStatus | null>(null);
  /** Whether this connection has held the seat. Reset with each connection. */
  const hadIt = ref(false);
  const busy = ref(false);
  const lastError = ref<string | null>(null);
  let claiming = false;

  const phase = computed<SeatPhase>(() => {
    const s = status.value;
    if (!s) return "unknown";
    if (s.held) return "held";
    if (s.claimable) return "claiming";
    return hadIt.value ? "taken" : "in-use";
  });

  /** A free seat is taken without asking; only one claim is ever in flight. */
  async function claim() {
    if (claiming) return;
    claiming = true;
    try {
      const outcome = await bus.call("seat.claim", {});
      if (outcome.status === "ok") apply(outcome.value as SeatStatus);
    } finally {
      claiming = false;
    }
  }

  function apply(next: SeatStatus) {
    status.value = next;
    if (next.held) {
      hadIt.value = true;
      lastError.value = null;
    } else if (next.claimable) {
      void claim();
    }
  }

  const off = bus.on((event) => {
    if (event.kind === "node") {
      // A new connection. Holding the seat on the old one, the workspace
      // stays while the node says whether the reconnect got it back (the
      // grace usually means it did); "unknown" also lets the window snapshot
      // that comes with the seat land. Not holding it, the takeover screen
      // stays as it was — "taken over" included — until the node says
      // otherwise, rather than mounting the workspace for a frame between
      // the welcome and the seat event, where every panel would ask for
      // things this device may not have.
      if (status.value?.held) status.value = null;
    } else if (event.kind === "seat") {
      apply(event.seat);
    }
  });

  return {
    status,
    phase,
    held: computed(() => phase.value === "held"),
    showWorkspace: computed(() => phase.value !== "in-use" && phase.value !== "taken"),
    device,
    busy,
    lastError,
    async takeOver() {
      const holder = status.value?.holder?.device ?? "another device";
      const ok = await ui.confirm({
        command: "seat.take",
        title: phase.value === "taken" ? "Take back Deckjack?" : "Take over Deckjack?",
        detail:
          `Deckjack is in use on ${holder}. Taking over moves the workspace — its windows ` +
          `and terminals — to this device, and ${holder} sees that it was taken over.`,
        risk: "write",
        action: phase.value === "taken" ? "Take back" : "Take over",
      });
      if (!ok) return false;
      busy.value = true;
      try {
        const outcome = await bus.call("seat.take", { device: device.label.value });
        if (outcome.status !== "ok") {
          lastError.value = "message" in outcome ? outcome.message : "the node did not hand over the seat";
          return false;
        }
        apply(outcome.value as SeatStatus);
        return status.value?.held === true;
      } finally {
        busy.value = false;
      }
    },
    async rename(label) {
      device.rename(label);
      // Seated, a take with the new name relabels this device for the
      // others; otherwise the name goes with the next take or connection.
      if (status.value?.held) {
        const outcome = await bus.call("seat.take", { device: device.label.value });
        if (outcome.status === "ok") apply(outcome.value as SeatStatus);
      }
    },
    dispose() {
      off();
    },
  };
}

/**
 * Keep the window store in step with the node's `windows` events, but only
 * while this connection does not hold the seat.
 *
 * Not holding it, the store follows every change, so a waiting device is
 * current; and the snapshot the node sends a connection just before handing
 * it the seat arrives while it does not hold it yet, so it lands too — which
 * is how a device that takes over shows exactly the windows that were open.
 *
 * Holding it, this connection is the one making the changes, and its
 * optimistic state is ahead of the node's echo of them: adopting an echo
 * mid-drag would snap a window back, and would drop what the store keeps
 * that the node does not (a file an app window was just asked to open).
 *
 * Returns the unsubscribe.
 */
export function followWindows(deps: { bus: Bus; windows: WindowsStore; seat: SeatStore }): () => void {
  return deps.bus.on((event) => {
    if (event.kind === "windows" && !deps.seat.held.value) deps.windows.adopt(event.windows);
  });
}

/**
 * When this connection loses the seat, close what floats above the
 * workspace: Quick Look, the palette and switcher, and any confirmation the
 * operator was answering (declined: the workspace it was about is no longer
 * theirs). The takeover screen replaces the workspace, but these are drawn
 * outside it and would otherwise stay on top of it, still working.
 *
 * Returns the unsubscribe.
 */
export function closeOverlaysOnLoss(deps: {
  seat: Pick<SeatStore, "showWorkspace">;
  ui: UiStore;
  quicklook: { close(): void; visible: Ref<boolean> };
}): () => void {
  // Synchronous: at the moment of the loss, so only what was open before it
  // is closed, never the takeover question the screen may open right after.
  return watch(
    deps.seat.showWorkspace,
    (show, before) => {
      if (show || !before) return;
      if (deps.quicklook.visible.value) deps.quicklook.close();
      deps.ui.paletteOpen.value = false;
      deps.ui.switcherOpen.value = false;
      deps.ui.confirmation.value?.resolve(false);
    },
    { flush: "sync" },
  );
}
