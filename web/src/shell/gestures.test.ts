import { describe, expect, it } from "vitest";

import { clampView, classifySwipe, FIT, pinchView, zoomAt, type View } from "./gestures.ts";

/**
 * Zoom, pan and swipe for Quick Look, as geometry.
 *
 * The promises are the ones a thumb notices: the point between two fingers
 * stays between them while they spread, a zoomed photo cannot be flung off
 * the screen, and a horizontal flick moves through the folder while a
 * vertical drag or a slow wobble does not.
 */

const stage = { w: 400, h: 800 };
/** The image as fitted to the stage at scale 1. */
const fitted = { w: 400, h: 300 };

/**
 * The image's edges on the stage under `view`. The view is a transform on a
 * stage-sized layer (origin at its top-left) in which the fitted image sits
 * centered.
 */
function edges(view: View) {
  const left = view.x + (view.scale * (stage.w - fitted.w)) / 2;
  const top = view.y + (view.scale * (stage.h - fitted.h)) / 2;
  return {
    left,
    top,
    right: left + view.scale * fitted.w,
    bottom: top + view.scale * fitted.h,
  };
}

/** Where a stage point lands on the content, under `view`. */
function contentAt(view: View, p: { x: number; y: number }) {
  return { x: (p.x - view.x) / view.scale, y: (p.y - view.y) / view.scale };
}

describe("pinching", () => {
  it("zooms by the ratio the fingers spread", () => {
    const start = [{ x: 150, y: 400 }, { x: 250, y: 400 }] as const;
    const now = [{ x: 100, y: 400 }, { x: 300, y: 400 }] as const;
    const v = pinchView(FIT, start, now, { min: 1, max: 8 });
    expect(v.scale).toBeCloseTo(2, 5);
  });

  it("keeps the point between the fingers under the fingers", () => {
    // Zooming toward the corner the operator is pinching, not toward the
    // middle of the screen, is the whole difference between a photo viewer
    // and a magnifier that drifts.
    const start = [{ x: 40, y: 100 }, { x: 80, y: 140 }] as const;
    const now = [{ x: 20, y: 80 }, { x: 100, y: 160 }] as const;
    const before = contentAt(FIT, { x: 60, y: 120 });
    const v = pinchView(FIT, start, now, { min: 1, max: 8 });
    const after = contentAt(v, { x: 60, y: 120 });
    expect(after.x).toBeCloseTo(before.x, 5);
    expect(after.y).toBeCloseTo(before.y, 5);
  });

  it("follows the fingers when they move together", () => {
    const start = [{ x: 100, y: 100 }, { x: 200, y: 100 }] as const;
    const now = [{ x: 130, y: 150 }, { x: 230, y: 150 }] as const;
    const v = pinchView({ scale: 2, x: 0, y: 0 }, start, now, { min: 1, max: 8 });
    expect(v.scale).toBeCloseTo(2, 5);
    expect([v.x, v.y]).toEqual([30, 50]);
  });

  it("stops at the limits rather than zooming without end", () => {
    const start = [{ x: 190, y: 400 }, { x: 210, y: 400 }] as const;
    const now = [{ x: 0, y: 400 }, { x: 400, y: 400 }] as const;
    expect(pinchView(FIT, start, now, { min: 1, max: 8 }).scale).toBe(8);
    expect(pinchView({ scale: 2, x: 0, y: 0 }, now, start, { min: 1, max: 8 }).scale).toBe(1);
  });
});

describe("zooming at a point", () => {
  it("keeps that point still, for a double tap or a wheel", () => {
    const at = { x: 300, y: 200 };
    const before = contentAt(FIT, at);
    const v = zoomAt(FIT, 2.5, at, { min: 1, max: 8 });
    expect(v.scale).toBe(2.5);
    const after = contentAt(v, at);
    expect(after.x).toBeCloseTo(before.x, 5);
    expect(after.y).toBeCloseTo(before.y, 5);
  });
});

describe("panning a zoomed image", () => {
  it("cannot drag the image off the screen", () => {
    // At 2x the image is 800x600 inside a 400x800 stage. Dragged far right
    // its left edge would leave a gap; it stops at the stage's edge instead.
    const right = edges(clampView({ scale: 2, x: 5000, y: 0 }, stage, fitted));
    expect(right.left).toBeCloseTo(0, 5);
    const left = edges(clampView({ scale: 2, x: -5000, y: 0 }, stage, fitted));
    expect(left.right).toBeCloseTo(stage.w, 5);
    // And anywhere in between is left where the finger put it.
    const middle = clampView({ scale: 2, x: -150, y: -400 }, stage, fitted);
    expect(middle.x).toBe(-150);
  });

  it("keeps an image smaller than the stage centered on that axis", () => {
    // 600 tall at 2x inside 800: there is nothing to pan to vertically.
    const e = edges(clampView({ scale: 2, x: 0, y: 300 }, stage, fitted));
    expect((e.top + e.bottom) / 2).toBeCloseTo(stage.h / 2, 5);
  });

  it("puts a fitted image back exactly where it started", () => {
    expect(clampView({ scale: 1, x: 40, y: -20 }, stage, fitted)).toEqual(FIT);
  });
});

describe("swiping through the folder", () => {
  it("goes to the next file on a flick to the left, the previous on a flick right", () => {
    expect(classifySwipe({ dx: -120, dy: 10, ms: 180 })).toBe("next");
    expect(classifySwipe({ dx: 120, dy: -8, ms: 180 })).toBe("previous");
  });

  it("ignores a vertical drag, which is scrolling", () => {
    expect(classifySwipe({ dx: 40, dy: 200, ms: 200 })).toBeNull();
    // Fast and with real sideways travel, but mostly down: still a scroll.
    expect(classifySwipe({ dx: 90, dy: 220, ms: 150 })).toBeNull();
  });

  it("ignores a short wobble", () => {
    expect(classifySwipe({ dx: 12, dy: 2, ms: 90 })).toBeNull();
  });

  it("accepts a slow drag that covers real distance", () => {
    expect(classifySwipe({ dx: -220, dy: 15, ms: 900 })).toBe("next");
  });

  it("ignores a slow, short drag", () => {
    expect(classifySwipe({ dx: -70, dy: 0, ms: 900 })).toBeNull();
  });
});
