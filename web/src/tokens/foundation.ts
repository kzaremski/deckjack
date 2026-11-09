import type { Foundation } from "./types.ts";

/**
 * The part of the token set that does not change with the theme.
 *
 * Type, space, radius, motion and density are shared because a theme is a
 * palette, not a layout: switching from Night to Circuit must not move a
 * single pixel. That is also what makes the density token safe to expose as
 * a global control.
 *
 * Fonts are self-hosted and embedded in the binary where the license allows.
 * Both stacks end in a system font so the shell is legible on the first paint
 * of a cold install, before any font has loaded.
 */
export const foundation: Foundation = {
  font: {
    sans: '"Space Grotesk", system-ui, -apple-system, "Segoe UI", Roboto, sans-serif',
    mono: '"JetBrains Mono", ui-monospace, "SF Mono", "Cascadia Mono", Menlo, monospace',
    display: '"Space Grotesk", system-ui, sans-serif',
    // Ligatures off: in a terminal and a diff, `!=` has to be two characters.
    monoFeatures: '"liga" 0, "calt" 0, "ss01" 1',
  },

  /**
   * Sizes are in `rem` so the operating system's text-size setting still
   * works, then multiplied by the density font scale. Line heights are
   * unitless so they survive that multiplication.
   */
  type: {
    micro: { size: "0.6875rem", line: "1.3", tracking: "0.04em", weight: 500 },
    small: { size: "0.8125rem", line: "1.4", tracking: "0", weight: 400 },
    body: { size: "0.875rem", line: "1.45", tracking: "0", weight: 400 },
    strong: { size: "0.875rem", line: "1.45", tracking: "0", weight: 600 },
    title: { size: "1rem", line: "1.35", tracking: "-0.005em", weight: 600 },
    heading: { size: "1.25rem", line: "1.25", tracking: "-0.01em", weight: 600 },
    display: { size: "1.625rem", line: "1.15", tracking: "-0.02em", weight: 600 },
    code: { size: "0.8125rem", line: "1.6", tracking: "0", weight: 400 },
  },

  /**
   * Multiples of the density unit rather than fixed pixels, which is what
   * makes compact mode tighter without every component being rewritten.
   */
  space: { 0: 0, 1: 1, 2: 2, 3: 3, 4: 4, 5: 5, 6: 6, 8: 8, 10: 10, 12: 12, 16: 16 },

  radius: {
    none: "0",
    xs: "3px",
    sm: "6px",
    md: "8px",
    lg: "12px",
    xl: "16px",
    pill: "999px",
  },

  // Hairlines are 1 physical pixel, which on a 2x or 3x screen is thinner
  // than 1 CSS pixel; without this the file list looks like a grid of boxes.
  stroke: { hairline: "max(0.5px, 1px / 2)", regular: "1px", thick: "2px" },

  motion: {
    duration: {
      instant: "0ms",
      fast: "90ms",
      base: "160ms",
      slow: "240ms",
      deliberate: "420ms",
    },
    easing: {
      standard: "cubic-bezier(0.2, 0, 0.2, 1)",
      entrance: "cubic-bezier(0.05, 0.7, 0.1, 1)",
      exit: "cubic-bezier(0.3, 0, 0.8, 0.15)",
      spring: "cubic-bezier(0.34, 1.26, 0.64, 1)",
    },
  },

  /**
   * Compact is the default: this is a workstation, and I would rather see
   * three more rows than more air. Touch targets never drop below 40px even
   * in compact, and `base.css` raises them on a coarse pointer, so density is
   * a comfort setting and not an accessibility cliff.
   */
  density: {
    compact: {
      label: "Compact",
      unit: "4px",
      control: "28px",
      touch: "40px",
      row: "30px",
      titlebar: "34px",
      fontScale: 1,
    },
    cozy: {
      label: "Cozy",
      unit: "5px",
      control: "34px",
      touch: "44px",
      row: "38px",
      titlebar: "38px",
      fontScale: 1.07,
    },
    comfortable: {
      label: "Comfortable",
      unit: "6px",
      control: "40px",
      touch: "48px",
      row: "46px",
      titlebar: "44px",
      fontScale: 1.14,
    },
  },

  /**
   * Widths where a class begins, measured in CSS pixels of the *viewport*,
   * not the device: a phone in DeX is a desktop and an unfolded Fold is a
   * small tablet. `foldFolded` is narrower than a typical phone on purpose —
   * the cover screen is the tightest layout anything has to survive.
   */
  breakpoint: {
    phone: 0,
    foldFolded: 360,
    foldUnfolded: 672,
    tablet: 840,
    desktop: 1200,
  },

  layer: {
    wall: 0,
    window: 10,
    windowFocused: 500,
    snapHint: 600,
    dock: 800,
    sheet: 850,
    palette: 900,
    toast: 950,
  },
};
