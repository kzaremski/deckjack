/**
 * Window geometry: snapping, clamping and resize math.
 *
 * Pure functions over plain rectangles, with no DOM and no store, because
 * this is the part of a window manager that is easy to get subtly wrong — a
 * window that can be dragged until only one pixel of its title bar is
 * reachable, a resize that inverts when it crosses its own edge — and easy to
 * check once it is separable.
 *
 * All units are CSS pixels inside the shell's work area: the viewport minus
 * the dock. Nothing here knows where that area is on screen.
 */

export interface Rect {
  x: number;
  y: number;
  w: number;
  h: number;
}

export interface Size {
  w: number;
  h: number;
}

/** The eight resize handles, named for compass directions. */
export type Handle = "n" | "s" | "e" | "w" | "ne" | "nw" | "se" | "sw";

/**
 * Where a drag can be released to tile. `maximize` is the top edge, the
 * quarters are the corners; the rest are halves. The names are what the
 * palette's tiling commands use too, so a keyboard snap and a dragged snap
 * cannot end up in different places.
 */
export type SnapZone =
  | "left"
  | "right"
  | "top"
  | "bottom"
  | "maximize"
  | "top-left"
  | "top-right"
  | "bottom-left"
  | "bottom-right";

/** How close to an edge a pointer has to get, in CSS pixels. */
export const SNAP_EDGE = 24;

/** How much of a corner counts as a corner rather than an edge. */
export const SNAP_CORNER = 120;

/** A window may never be dragged so far that less than this is reachable. */
export const MIN_VISIBLE = 96;

export const DEFAULT_MIN_SIZE: Size = { w: 320, h: 180 };

export function rectsEqual(a: Rect, b: Rect): boolean {
  return a.x === b.x && a.y === b.y && a.w === b.w && a.h === b.h;
}

export function round(rect: Rect): Rect {
  return {
    x: Math.round(rect.x),
    y: Math.round(rect.y),
    w: Math.round(rect.w),
    h: Math.round(rect.h),
  };
}

/**
 * Keeps a window reachable rather than keeping it inside the work area.
 *
 * Fully constraining a window is the wrong behavior: pushing a wide window
 * partly off the left edge to read the right half of it is a real thing to
 * want. What must never happen is losing the title bar, since that is the
 * only handle it has.
 */
export function clampToWorkArea(rect: Rect, area: Size, titlebar: number): Rect {
  const maxX = area.w - MIN_VISIBLE;
  const minX = MIN_VISIBLE - rect.w;
  return {
    x: Math.min(maxX, Math.max(minX, rect.x)),
    // The title bar stays on screen top and bottom: above zero it would be
    // under nothing, below the area it would be under the dock.
    y: Math.min(area.h - titlebar, Math.max(0, rect.y)),
    w: rect.w,
    h: rect.h,
  };
}

/** The zone a pointer at `(x, y)` would snap to, or null for a free drop. */
export function snapZoneAt(x: number, y: number, area: Size): SnapZone | null {
  const nearLeft = x <= SNAP_EDGE;
  const nearRight = x >= area.w - SNAP_EDGE;
  const nearTop = y <= SNAP_EDGE;
  const nearBottom = y >= area.h - SNAP_EDGE;

  if (nearTop && nearLeft) return "top-left";
  if (nearTop && nearRight) return "top-right";
  if (nearBottom && nearLeft) return "bottom-left";
  if (nearBottom && nearRight) return "bottom-right";

  // A corner is also reachable by sliding along an edge, which is how this
  // gets used in practice once the edge itself is already a half.
  if (nearLeft) return y < SNAP_CORNER ? "top-left" : y > area.h - SNAP_CORNER ? "bottom-left" : "left";
  if (nearRight) {
    return y < SNAP_CORNER ? "top-right" : y > area.h - SNAP_CORNER ? "bottom-right" : "right";
  }
  if (nearTop) return "maximize";
  if (nearBottom) return "bottom";
  return null;
}

/** The rectangle a zone means, inset by `gap` so tiled windows keep a seam. */
export function rectForZone(zone: SnapZone, area: Size, gap = 0): Rect {
  const halfW = Math.floor((area.w - gap * 3) / 2);
  const halfH = Math.floor((area.h - gap * 3) / 2);
  const rightX = gap * 2 + halfW;
  const bottomY = gap * 2 + halfH;
  const fullW = area.w - gap * 2;
  const fullH = area.h - gap * 2;

  switch (zone) {
    case "maximize":
      return { x: gap, y: gap, w: fullW, h: fullH };
    case "left":
      return { x: gap, y: gap, w: halfW, h: fullH };
    case "right":
      return { x: rightX, y: gap, w: halfW, h: fullH };
    case "top":
      return { x: gap, y: gap, w: fullW, h: halfH };
    case "bottom":
      return { x: gap, y: bottomY, w: fullW, h: halfH };
    case "top-left":
      return { x: gap, y: gap, w: halfW, h: halfH };
    case "top-right":
      return { x: rightX, y: gap, w: halfW, h: halfH };
    case "bottom-left":
      return { x: gap, y: bottomY, w: halfW, h: halfH };
    case "bottom-right":
      return { x: rightX, y: bottomY, w: halfW, h: halfH };
  }
}

/**
 * Applies a resize drag to a rectangle.
 *
 * The minimum size is enforced by moving the opposite edge, never by letting
 * the dragged edge pass through it, so dragging a west handle far to the
 * right leaves the window pinned at its minimum width with its east edge
 * where it was.
 */
export function resizeRect(
  rect: Rect,
  handle: Handle,
  dx: number,
  dy: number,
  min: Size = DEFAULT_MIN_SIZE,
): Rect {
  let { x, y, w, h } = rect;

  if (handle.includes("e")) w = Math.max(min.w, w + dx);
  if (handle.includes("s")) h = Math.max(min.h, h + dy);
  if (handle.includes("w")) {
    const next = Math.max(min.w, w - dx);
    x = x + (w - next);
    w = next;
  }
  if (handle.includes("n")) {
    const next = Math.max(min.h, h - dy);
    y = y + (h - next);
    h = next;
  }
  return { x, y, w, h };
}

/** The cursor for a handle, so the hit areas and the cursors stay in step. */
export function cursorFor(handle: Handle): string {
  switch (handle) {
    case "n":
    case "s":
      return "ns-resize";
    case "e":
    case "w":
      return "ew-resize";
    case "ne":
    case "sw":
      return "nesw-resize";
    case "nw":
    case "se":
      return "nwse-resize";
  }
}

/**
 * Where a newly opened window goes: stepped down and right from the last one,
 * wrapping before it walks off the work area. Opening six terminals should
 * leave six reachable title bars, not a stack of one.
 */
export function cascade(index: number, area: Size, size: Size, step = 32): Rect {
  const perColumn = Math.max(1, Math.floor((area.h - size.h - 40) / step) || 1);
  const column = Math.floor(index / perColumn);
  const row = index % perColumn;
  const x = 40 + column * (step * 3) + row * step;
  const y = 32 + row * step;
  return clampToWorkArea(
    { x: Math.min(x, Math.max(0, area.w - size.w - 16)), y, w: size.w, h: size.h },
    area,
    34,
  );
}

/** Lays `count` windows out in one row, for the "tile side by side" command. */
export function tileRow(count: number, area: Size, gap = 8): Rect[] {
  if (count <= 0) return [];
  const w = Math.floor((area.w - gap * (count + 1)) / count);
  return Array.from({ length: count }, (_, i) => ({
    x: gap + i * (w + gap),
    y: gap,
    w,
    h: area.h - gap * 2,
  }));
}

/**
 * Master and stack, the way i3 and Hyprland open into a split: the first
 * window takes half, the rest divide the other half between them.
 *
 * `tileRow` stays for the one-shot "tile side by side" command, where a single
 * row is exactly what was asked for. It is the wrong shape for a *persistent*
 * tiling mode: the columns shrink without bound while the height never
 * changes, so six windows become six unusable slivers.
 *
 * One window takes the whole area rather than half, because a master with an
 * empty stack beside it is just a window wasting half the screen.
 */
export function tileMasterStack(count: number, area: Size, gap = 8): Rect[] {
  if (count <= 0) return [];
  if (count === 1) {
    return [{ x: gap, y: gap, w: area.w - gap * 2, h: area.h - gap * 2 }];
  }

  const stack = count - 1;
  // Split along the long axis, so a portrait panel — the Fold unfolded, this
  // machine's rotated LCD — stacks top-and-bottom instead of producing two
  // tall ribbons.
  if (area.h > area.w) {
    const masterH = Math.floor((area.h - gap * 3) / 2);
    const cell = Math.floor((area.w - gap * (stack + 1)) / stack);
    return [
      { x: gap, y: gap, w: area.w - gap * 2, h: masterH },
      ...Array.from({ length: stack }, (_, i) => ({
        x: gap + i * (cell + gap),
        y: gap * 2 + masterH,
        w: cell,
        h: area.h - masterH - gap * 3,
      })),
    ];
  }

  const masterW = Math.floor((area.w - gap * 3) / 2);
  const cell = Math.floor((area.h - gap * (stack + 1)) / stack);
  return [
    { x: gap, y: gap, w: masterW, h: area.h - gap * 2 },
    ...Array.from({ length: stack }, (_, i) => ({
      x: gap * 2 + masterW,
      y: gap + i * (cell + gap),
      w: area.w - masterW - gap * 3,
      h: cell,
    })),
  ];
}
