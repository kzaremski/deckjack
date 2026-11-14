import { describe, expect, it } from "vitest";

import { tileMasterStack, type Rect, type Size } from "./geometry.ts";

/**
 * What a tiled layout promises, stated as properties rather than coordinates.
 *
 * Asserting exact rectangles would pin one arrangement and call it correct, so
 * every future change to the layout would "fail" even when it was an
 * improvement — and, worse, a wrong arrangement with the right numbers would
 * pass. What a tiling layout actually owes its user is this: nothing overlaps,
 * nothing escapes the screen, nothing is too small to use, and the space is
 * not mostly wasted. Those hold for any sensible arrangement and fail for
 * every broken one.
 */

function overlaps(a: Rect, b: Rect): boolean {
  return a.x < b.x + b.w && b.x < a.x + a.w && a.y < b.y + b.h && b.y < a.y + a.h;
}

/** Takes anything with width and height, so a screen and a tile both work. */
function area(r: { w: number; h: number }): number {
  return r.w * r.h;
}

const LANDSCAPE: Size = { w: 1600, h: 900 };
const PORTRAIT: Size = { w: 900, h: 1600 };
const GAP = 6;

describe("master and stack", () => {
  it("never puts two windows on top of each other", () => {
    for (const count of [2, 3, 4, 5, 6, 9]) {
      const rects = tileMasterStack(count, LANDSCAPE, GAP);
      for (let i = 0; i < rects.length; i++) {
        for (let j = i + 1; j < rects.length; j++) {
          expect(
            overlaps(rects[i]!, rects[j]!),
            `windows ${i} and ${j} overlap with ${count} open`,
          ).toBe(false);
        }
      }
    }
  });

  it("keeps every window on the screen", () => {
    for (const count of [1, 2, 3, 7]) {
      for (const size of [LANDSCAPE, PORTRAIT]) {
        for (const r of tileMasterStack(count, size, GAP)) {
          expect(r.x).toBeGreaterThanOrEqual(0);
          expect(r.y).toBeGreaterThanOrEqual(0);
          expect(r.x + r.w, `a window ran off the right with ${count} open`).toBeLessThanOrEqual(
            size.w,
          );
          expect(r.y + r.h, `a window ran off the bottom with ${count} open`).toBeLessThanOrEqual(
            size.h,
          );
        }
      }
    }
  });

  it("uses most of the screen rather than leaving it mostly empty", () => {
    // The failure this guards against is a layout that technically tiles but
    // packs everything into a corner.
    for (const count of [1, 2, 3, 5]) {
      const used = tileMasterStack(count, LANDSCAPE, GAP).reduce((sum, r) => sum + area(r), 0);
      expect(used / area(LANDSCAPE), `${count} windows used too little of the screen`).toBeGreaterThan(
        0.85,
      );
    }
  });

  it("gives a lone window the whole screen, not half of it", () => {
    const [only] = tileMasterStack(1, LANDSCAPE, GAP);
    expect(only).toBeTruthy();
    expect(area(only!) / area(LANDSCAPE)).toBeGreaterThan(0.95);
  });

  it("keeps every window usable instead of shrinking them without limit", () => {
    // Six windows in a single row is the arrangement this layout exists to
    // avoid: the columns divide width without bound, so each one becomes a
    // sliver far too narrow to read.
    const rects = tileMasterStack(6, LANDSCAPE, GAP);
    for (const r of rects) {
      expect(r.w, "a tile was too narrow to use").toBeGreaterThan(LANDSCAPE.w / 6);
    }
  });

  it("splits a tall screen top-and-bottom, not into tall ribbons", () => {
    // A portrait panel — the Fold unfolded, this machine's rotated LCD — must
    // not be cut into vertical strips.
    const [master, second] = tileMasterStack(2, PORTRAIT, GAP);
    expect(master).toBeTruthy();
    expect(second).toBeTruthy();
    expect(master!.w, "the master was cut narrow on a tall screen").toBeGreaterThan(
      PORTRAIT.w * 0.9,
    );
    expect(second!.y, "the second window was placed beside rather than below").toBeGreaterThan(
      master!.y,
    );
  });

  it("asks for nothing when there is nothing open", () => {
    expect(tileMasterStack(0, LANDSCAPE, GAP)).toEqual([]);
  });
});
