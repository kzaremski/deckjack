import { circuit } from "./themes/circuit.ts";
import { night } from "./themes/night.ts";
import { foundation } from "./foundation.ts";
import type { Theme, ThemeName, TokenManifest } from "./types.ts";

/**
 * Every theme the shell knows, keyed by name. A new theme is one file plus one
 * line here; nothing else enumerates themes.
 */
export const themes: Record<ThemeName, Theme> = { night, circuit };

export const DEFAULT_THEME: ThemeName = "night";

export const themeList: Theme[] = Object.values(themes);

export function isThemeName(value: string): value is ThemeName {
  return value in themes;
}

/**
 * What a generator or the Rust side is handed. Kept as one serializable
 * object so `deckjack` can embed it and hand the same bytes to a canvas
 * surface, an xterm theme and a Monaco theme.
 */
export const tokenManifest: TokenManifest = {
  version: 1,
  foundation,
  themes,
};
