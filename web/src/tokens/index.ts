/**
 * The design system's public surface. Everything that styles a Deckjack
 * surface comes from here: there is no second palette, in this repo or in an
 * app binary's UI.
 */
export * from "./types.ts";
export { foundation } from "./foundation.ts";
export { DEFAULT_THEME, isThemeName, themeList, themes, tokenManifest } from "./registry.ts";
export {
  PREFIX,
  cssVar,
  densityCss,
  flatten,
  foundationCss,
  paletteVariables,
  themeCss,
  tokenStylesheet,
  varName,
} from "./css.ts";
export {
  browserThemeEnv,
  createThemeController,
  installTokenStylesheet,
  resolvePalette,
  resolveToken,
  safeStorage,
  useCanvasTokens,
  type ThemeController,
  type ThemeEnv,
} from "./runtime.ts";
