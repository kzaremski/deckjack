import { describe, expect, it } from "vitest";

import {
  clampToWorkArea,
  rectForZone,
  tileMasterStack,
  type Rect,
  type Size,
  type SnapZone,
} from "./geometry.ts";

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

/**
 * Which half of the screen a zone name refers to, across and down.
 *
 * This is the whole promise of a snap zone: `bottom` is the bottom, `top-left`
 * is the top-left, and the names are shared with the palette's tiling commands
 * and with the drag overlay, so a window dropped at the bottom edge and a
 * window sent there by the keyboard have to land in the same place. Stating it
 * as a band rather than a rectangle keeps the seam, the rounding and any future
 * change to the gap free to move.
 */
type Band = "start" | "end" | "full";

const WHERE: Record<SnapZone, { across: Band; down: Band }> = {
  maximize: { across: "full", down: "full" },
  left: { across: "start", down: "full" },
  right: { across: "end", down: "full" },
  top: { across: "full", down: "start" },
  bottom: { across: "full", down: "end" },
  "top-left": { across: "start", down: "start" },
  "top-right": { across: "end", down: "start" },
  "bottom-left": { across: "start", down: "end" },
  "bottom-right": { across: "end", down: "end" },
};

/**
 * Whether an extent along one axis sits in the named band. The slack is the
 * seam plus the half-pixel the halves round away, which is what lets a layout
 * change its gap without this becoming a coordinate assertion.
 */
function inBand(start: number, extent: number, total: number, band: Band, seam: number): boolean {
  const slack = seam * 2 + 1;
  switch (band) {
    case "start":
      return start <= slack && start + extent <= total / 2 + slack && extent >= total * 0.4;
    case "end":
      return start >= total / 2 - slack && start + extent <= total && extent >= total * 0.4;
    case "full":
      return start <= slack && start + extent >= total - slack;
  }
}

describe("the rectangle a snap zone names", () => {
  it("puts the window in the half or quarter the zone is named after", () => {
    for (const screen of [LANDSCAPE, PORTRAIT]) {
      for (const seam of [0, GAP]) {
        for (const zone of Object.keys(WHERE) as SnapZone[]) {
          const r = rectForZone(zone, screen, seam);
          const shape = `${zone} on ${screen.w}x${screen.h} with a ${seam}px seam gave ${JSON.stringify(r)}`;
          expect(
            inBand(r.x, r.w, screen.w, WHERE[zone].across, seam),
            `${shape}: that is not the side of the screen the zone is named after`,
          ).toBe(true);
          expect(
            inBand(r.y, r.h, screen.h, WHERE[zone].down, seam),
            `${shape}: that is not the part of the screen, top to bottom, that the zone names`,
          ).toBe(true);
        }
      }
    }
  });

  it("divides the screen between the four corner zones", () => {
    // Independent of the band check above, and for a different failure: two
    // corners that resolve to the same quarter leave half the screen
    // unreachable by dragging, and two windows on top of each other.
    const corners = (["top-left", "top-right", "bottom-left", "bottom-right"] as const).map(
      (zone) => ({ zone, rect: rectForZone(zone, LANDSCAPE, GAP) }),
    );

    for (let i = 0; i < corners.length; i++) {
      for (let j = i + 1; j < corners.length; j++) {
        expect(
          overlaps(corners[i]!.rect, corners[j]!.rect),
          `${corners[i]!.zone} and ${corners[j]!.zone} are the same part of the screen`,
        ).toBe(false);
      }
    }

    const used = corners.reduce((sum, c) => sum + area(c.rect), 0);
    expect(used / area(LANDSCAPE), "the four corners do not add up to the screen").toBeGreaterThan(
      0.9,
    );
  });
});

/**
 * How much of a window must stay on screen to be grabbable at all. Stated in
 * human terms on purpose: 80px is about the width of a title you can put a
 * pointer or a thumb on without hunting for it, and at the bottom edge it has
 * to clear the rounded corner and the dock. It is deliberately NOT written as
 * `MIN_VISIBLE`, so dropping that constant to a sliver fails here instead of
 * being quietly ratified.
 */
const GRABBABLE = 80;
const TITLEBAR = 34;

describe("keeping a thrown window reachable", () => {
  const sizes: Size[] = [
    { w: 320, h: 180 },
    { w: 680, h: 440 },
    // Wider and taller than the screen: a window pushed partly off the left
    // edge to read its right half is a real thing to want, and must still be
    // grabbable.
    { w: 2000, h: 1500 },
  ];

  it("leaves a strip of the window on screen however far it is thrown", () => {
    for (const screen of [LANDSCAPE, PORTRAIT]) {
      for (const size of sizes) {
        for (const x of [-9000, -size.w, 0, screen.w, 9000]) {
          const at = clampToWorkArea({ x, y: 100, ...size }, screen, TITLEBAR);
          const onScreen = Math.min(at.x + at.w, screen.w) - Math.max(at.x, 0);
          expect(
            onScreen,
            `a ${size.w}x${size.h} window thrown to x=${x} left only ${onScreen}px to grab`,
          ).toBeGreaterThanOrEqual(GRABBABLE);
        }
      }
    }
  });

  it("keeps the title bar row between the top of the work area and its bottom edge", () => {
    for (const screen of [LANDSCAPE, PORTRAIT]) {
      for (const size of sizes) {
        for (const y of [-9000, -size.h, 0, screen.h, 9000]) {
          const at = clampToWorkArea({ x: 100, y, ...size }, screen, TITLEBAR);
          expect(at.y, `a window thrown to y=${y} put its title bar above the screen`).toBeGreaterThanOrEqual(
            0,
          );
          expect(
            at.y + TITLEBAR,
            `a window thrown to y=${y} put its title bar under the dock`,
          ).toBeLessThanOrEqual(screen.h);
        }
      }
    }
  });
});
