/**
 * The token vocabulary.
 *
 * One token set styles every surface; nothing gets its own palette. That rule
 * only holds if the vocabulary is written down once, so these types are the
 * contract that the themes, the CSS emitter, the canvas surfaces and the
 * generators for xterm, Monaco, mermaid and code-server all read.
 *
 * Everything here is plain data: strings and numbers, no functions, no DOM.
 * A generator must be able to `JSON.stringify` a theme and hand it to a tool
 * that has never heard of Vue, and the Rust side has to be able to read the
 * same JSON for canvas and WebGPU surfaces.
 */

export type ThemeName = "night" | "circuit";

/** Which built-in form controls and scrollbars a theme wants. */
export type ColorScheme = "dark" | "light";

export type DensityName = "compact" | "cozy" | "comfortable";

export type TypeStep =
  | "micro"
  | "small"
  | "body"
  | "strong"
  | "title"
  | "heading"
  | "display"
  | "code";

export type SpaceStep = 0 | 1 | 2 | 3 | 4 | 5 | 6 | 8 | 10 | 12 | 16;

export type RadiusStep = "none" | "xs" | "sm" | "md" | "lg" | "xl" | "pill";

export type BreakpointName =
  | "phone"
  | "foldFolded"
  | "foldUnfolded"
  | "tablet"
  | "desktop";

export type LayerName =
  | "wall"
  | "window"
  | "windowFocused"
  | "snapHint"
  | "dock"
  | "sheet"
  | "palette"
  | "toast";

/** The 16 ANSI colors, in the order a terminal expects them. */
export interface AnsiColors {
  black: string;
  red: string;
  green: string;
  yellow: string;
  blue: string;
  magenta: string;
  cyan: string;
  white: string;
  brightBlack: string;
  brightRed: string;
  brightGreen: string;
  brightYellow: string;
  brightBlue: string;
  brightMagenta: string;
  brightCyan: string;
  brightWhite: string;
}

/**
 * Terminal colors live in the theme rather than in the terminal panel,
 * because the panel is a surface like any other: an xterm whose background
 * differs from the window it sits in reads as a hole in the desktop.
 */
export interface TerminalColors {
  background: string;
  foreground: string;
  cursor: string;
  cursorText: string;
  selection: string;
  selectionText: string;
  ansi: AnsiColors;
}

/**
 * Syntax roles, not language keywords. The Monaco and code-server theme
 * generators map editor scopes onto these; nothing downstream invents a
 * color of its own.
 */
export interface SyntaxColors {
  comment: string;
  keyword: string;
  control: string;
  operator: string;
  punctuation: string;
  string: string;
  escape: string;
  number: string;
  constant: string;
  variable: string;
  property: string;
  parameter: string;
  function: string;
  type: string;
  tag: string;
  attribute: string;
  invalid: string;
  diffAdded: string;
  diffRemoved: string;
  diffChanged: string;
}

/**
 * What canvas and WebGPU surfaces draw with. A FilmCraft timeline or an audio
 * waveform has to be indistinguishable in style from the DOM around it, so it
 * reads these at runtime instead of hard-coding anything.
 */
export interface CanvasColors {
  grid: string;
  gridStrong: string;
  axis: string;
  playhead: string;
  waveform: string;
  waveformPeak: string;
  clipVideo: string;
  clipAudio: string;
  clipSelected: string;
  scopeTrace: string;
  scopeClip: string;
}

/**
 * Risk is declared by the backend per command, never inferred by the UI. The
 * UI only owns how each level looks, and it looks the same in the palette, on
 * a confirmation and on an Inbox card.
 */
export interface RiskColors {
  read: string;
  write: string;
  destructive: string;
}

export interface ShadowTokens {
  low: string;
  mid: string;
  high: string;
  inset: string;
}

export interface Palette {
  /** Behind everything: the desktop wall. */
  ground: string;
  /** The wall's pattern lines, if a theme has one. */
  groundPattern: string;
  /** A window, a dock, a panel. */
  surface: string;
  /** A title bar, a card, a popover: one step toward the viewer. */
  raised: string;
  /** An input, a well, a terminal: one step away. */
  sunken: string;
  /** The palette and sheets, which float above windows. */
  overlay: string;
  /** What dims the desktop behind an overlay. */
  scrim: string;

  /** Table rules and other lines that must not read as borders. */
  hairline: string;
  border: string;
  borderStrong: string;
  focusRing: string;

  text: string;
  textMuted: string;
  textDim: string;
  /** Text on top of `accent`. */
  textOnAccent: string;

  accent: string;
  accentHover: string;
  accentSoft: string;
  accentLine: string;

  ok: string;
  okSoft: string;
  warn: string;
  warnSoft: string;
  danger: string;
  dangerSoft: string;
  info: string;
  infoSoft: string;

  /** Text on `warn` and `danger` fills, which are light in both themes. */
  textOnWarn: string;
  textOnDanger: string;

  selection: string;
  /** The skeleton base and its sweep, used instead of spinners. */
  skeleton: string;
  skeletonSheen: string;

  risk: RiskColors;
  shadow: ShadowTokens;
  terminal: TerminalColors;
  syntax: SyntaxColors;
  canvas: CanvasColors;
  /** Categorical colors for charts, scopes and per-node coloring, in order. */
  series: string[];
}

export interface Theme {
  name: ThemeName;
  label: string;
  scheme: ColorScheme;
  /** The browser UI color for this theme: PWA title bar, Android chrome. */
  browserColor: string;
  color: Palette;
}

export interface TypeToken {
  size: string;
  line: string;
  tracking: string;
  weight: number;
}

export interface DensityTokens {
  label: string;
  /** The spacing unit every `space` token is a multiple of. */
  unit: string;
  /** Height of a button, an input, a dock item. */
  control: string;
  /** The minimum hit area, which never shrinks below the control height. */
  touch: string;
  /** A list row: files, mail, palette results. */
  row: string;
  /** A window title bar. */
  titlebar: string;
  /** Multiplies the type scale, so density changes text as well as space. */
  fontScale: number;
}

export interface Foundation {
  font: {
    sans: string;
    mono: string;
    display: string;
    /** Applied to monospace surfaces; tabular figures everywhere else. */
    monoFeatures: string;
  };
  type: Record<TypeStep, TypeToken>;
  space: Record<SpaceStep, number>;
  radius: Record<RadiusStep, string>;
  stroke: { hairline: string; regular: string; thick: string };
  motion: {
    duration: {
      instant: string;
      fast: string;
      base: string;
      slow: string;
      deliberate: string;
    };
    easing: {
      standard: string;
      entrance: string;
      exit: string;
      spring: string;
    };
  };
  density: Record<DensityName, DensityTokens>;
  /** Minimum width in CSS pixels at which each class begins. */
  breakpoint: Record<BreakpointName, number>;
  layer: Record<LayerName, number>;
}

/** What a generator (or the Rust side) is handed: everything, once. */
export interface TokenManifest {
  version: 1;
  foundation: Foundation;
  themes: Record<ThemeName, Theme>;
}
