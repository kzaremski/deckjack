import { computed, ref, type ComputedRef, type Ref } from "vue";

import { foundation } from "../tokens/index.ts";
import type { BreakpointName } from "../tokens/index.ts";

/**
 * Which layout the shell is in, and why.
 *
 * Mode follows the viewport and the input, never the device: a phone in DeX
 * is a desktop, an unfolded Fold is a small tablet, and a 13" laptop with a
 * touchscreen is still a desktop because it has a pointer. Sniffing the user
 * agent would get all three wrong.
 */

export type ShellMode = "desktop" | "compact";

export interface ViewportEnv {
  media(query: string): MediaQueryList;
  /** Current viewport size in CSS pixels. */
  size(): { w: number; h: number };
  /** Subscribes to size changes; returns the unsubscribe. */
  onResize(fn: () => void): () => void;
  /**
   * The fraction of the width where a foldable's hinge sits, or null when
   * there is no hinge. Separated out because measuring it needs a probe
   * element and both the measurement and the fallback deserve to be visible.
   */
  hingeFraction(): number | null;
}

const ORDER: BreakpointName[] = [
  "phone",
  "foldFolded",
  "foldUnfolded",
  "tablet",
  "desktop",
];

export function breakpointFor(width: number): BreakpointName {
  let match: BreakpointName = "phone";
  for (const name of ORDER) {
    if (width >= foundation.breakpoint[name]) match = name;
  }
  return match;
}

/**
 * Reads the hinge position from the CSS viewport segment environment
 * variables, which are the only honest source for it: the Device Posture and
 * Viewport Segments APIs are not everywhere, and guessing the middle is wrong
 * on an asymmetric fold.
 */
export function measureHinge(doc: Document, win: Window): number | null {
  if (!win.matchMedia("(horizontal-viewport-segments: 2)").matches) return null;
  const probe = doc.createElement("div");
  probe.style.cssText =
    "position:fixed;top:0;height:1px;width:1px;visibility:hidden;" +
    "left:env(viewport-segment-right 0 0, 50%)";
  doc.body.appendChild(probe);
  const left = probe.getBoundingClientRect().left;
  probe.remove();
  const width = win.innerWidth || 1;
  const fraction = left / width;
  // A probe that landed at zero means the env() was not understood after
  // all; the hinge is never at the very edge.
  return fraction > 0.05 && fraction < 0.95 ? fraction : 0.5;
}

export function browserViewportEnv(): ViewportEnv {
  return {
    media: (q) => window.matchMedia(q),
    size: () => ({
      // `visualViewport` is what shrinks when a soft keyboard opens, which is
      // the size a compact layout actually has to fit into.
      w: Math.round(window.visualViewport?.width ?? window.innerWidth),
      h: Math.round(window.visualViewport?.height ?? window.innerHeight),
    }),
    onResize(fn) {
      window.addEventListener("resize", fn, { passive: true });
      window.visualViewport?.addEventListener("resize", fn, { passive: true });
      return () => {
        window.removeEventListener("resize", fn);
        window.visualViewport?.removeEventListener("resize", fn);
      };
    },
    hingeFraction: () => measureHinge(document, window),
  };
}

export interface Viewport {
  width: Ref<number>;
  height: Ref<number>;
  breakpoint: ComputedRef<BreakpointName>;
  /** True when the primary input is a finger or a pen rather than a mouse. */
  coarsePointer: Ref<boolean>;
  /** What the viewport and input imply, ignoring any override. */
  suggestedMode: ComputedRef<ShellMode>;
  /** What the shell is actually in. */
  mode: ComputedRef<ShellMode>;
  /** Set by the operator; clears back to automatic with null. */
  override: Ref<ShellMode | null>;
  /** Where to put a split's divider by default: the hinge, or the middle. */
  splitFraction: ComputedRef<number>;
  hinge: Ref<number | null>;
  /** True when running as an installed PWA in its own window. */
  standalone: Ref<boolean>;
  stop(): void;
}

export function createViewport(env: ViewportEnv): Viewport {
  const initial = env.size();
  const width = ref(initial.w);
  const height = ref(initial.h);
  const hinge = ref<number | null>(env.hingeFraction());
  const override = ref<ShellMode | null>(null);

  const coarse = env.media("(pointer: coarse)");
  const coarsePointer = ref(coarse.matches);
  const onCoarse = (e: MediaQueryListEvent) => {
    coarsePointer.value = e.matches;
  };
  coarse.addEventListener("change", onCoarse);

  const display = env.media("(display-mode: standalone), (display-mode: window-controls-overlay)");
  const standalone = ref(display.matches);
  const onDisplay = (e: MediaQueryListEvent) => {
    standalone.value = e.matches;
  };
  display.addEventListener("change", onDisplay);

  const stopResize = env.onResize(() => {
    const s = env.size();
    width.value = s.w;
    height.value = s.h;
    // Unfolding changes both the size and whether there is a hinge, and the
    // resize is the only event that reliably fires for it.
    hinge.value = env.hingeFraction();
  });

  /**
   * Releases everything, not just the resize subscription.
   *
   * The media-query listeners used to be left attached, so a stopped viewport
   * went on mutating its own state and every shell rebuild -- a hot reload in
   * development, a reconnect that rebuilds the stores -- left another pair of
   * them behind, each holding its closure alive.
   */
  const stop = () => {
    stopResize();
    coarse.removeEventListener("change", onCoarse);
    display.removeEventListener("change", onDisplay);
  };

  const suggestedMode = computed<ShellMode>(() =>
    width.value >= foundation.breakpoint.tablet && !coarsePointer.value
      ? "desktop"
      : "compact",
  );

  return {
    width,
    height,
    breakpoint: computed(() => breakpointFor(width.value)),
    coarsePointer,
    suggestedMode,
    mode: computed(() => override.value ?? suggestedMode.value),
    override,
    hinge,
    splitFraction: computed(() => hinge.value ?? 0.5),
    standalone,
    stop,
  };
}
