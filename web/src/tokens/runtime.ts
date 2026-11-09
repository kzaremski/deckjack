import { computed, ref, shallowRef, type Ref } from "vue";

import { flatten, tokenStylesheet, varName } from "./css.ts";
import { foundation } from "./foundation.ts";
import { DEFAULT_THEME, isThemeName, themes } from "./registry.ts";
import type { DensityName, Palette, Theme, ThemeName } from "./types.ts";

const STYLE_ID = "dj-tokens";
const THEME_KEY = "deckjack.theme";
const DENSITY_KEY = "deckjack.density";

/**
 * What the theme controller needs from the outside world.
 *
 * Injected rather than read ambiently for the same reason `lock.rs` injects
 * its clock: a controller that reaches for `document` and `localStorage`
 * itself cannot be driven from a test, and `localStorage` is not merely
 * missing in a private window — reading it throws.
 */
export interface ThemeEnv {
  root: HTMLElement;
  /** Where the stylesheet is installed; usually `root.ownerDocument`. */
  document: Document;
  storage: {
    read(key: string): string | null;
    write(key: string, value: string): void;
  };
  /** `window.matchMedia`, or a stub. */
  media(query: string): MediaQueryList;
}

/** A storage shim that degrades to in-memory rather than throwing. */
export function safeStorage(): ThemeEnv["storage"] {
  const memory = new Map<string, string>();
  return {
    read(key) {
      try {
        return window.localStorage.getItem(key);
      } catch {
        return memory.get(key) ?? null;
      }
    },
    write(key, value) {
      memory.set(key, value);
      try {
        window.localStorage.setItem(key, value);
      } catch {
        // A private window or blocked site data. The preference is still
        // live for this session; losing it on reload is the right trade
        // against failing to apply a theme at all.
      }
    },
  };
}

export function browserThemeEnv(): ThemeEnv {
  return {
    root: document.documentElement,
    document,
    storage: safeStorage(),
    media: (q) => window.matchMedia(q),
  };
}

/**
 * Installs the generated token stylesheet, replacing any earlier copy.
 *
 * It goes in first, before component styles, so a component can override a
 * token locally (a window in a highlighted state, say) without `!important`.
 */
export function installTokenStylesheet(doc: Document): HTMLStyleElement {
  const existing = doc.getElementById(STYLE_ID);
  if (existing instanceof HTMLStyleElement) {
    existing.textContent = tokenStylesheet();
    return existing;
  }
  const style = doc.createElement("style");
  style.id = STYLE_ID;
  style.textContent = tokenStylesheet();
  doc.head.prepend(style);
  return style;
}

function isDensityName(value: string): value is DensityName {
  return value in foundation.density;
}

export interface ThemeController {
  theme: Ref<ThemeName>;
  density: Ref<DensityName>;
  /** The resolved theme object, for anything that needs raw values. */
  current: Ref<Theme>;
  /** True while the theme is whatever the system asks for. */
  followSystem: Ref<boolean>;
  setTheme(name: ThemeName): void;
  setDensity(name: DensityName): void;
  /** A cheap counter anything cached off tokens can watch. */
  revision: Ref<number>;
}

/**
 * Owns the two attributes on `<html>` that every surface reads.
 *
 * A stored preference wins over the system's; without one, the system's
 * light/dark choice picks between Circuit and Night, so a laptop that dims
 * at sunset does the right thing with no setting at all.
 */
export function createThemeController(env: ThemeEnv): ThemeController {
  const dark = env.media("(prefers-color-scheme: dark)");
  const storedTheme = env.storage.read(THEME_KEY);
  const storedDensity = env.storage.read(DENSITY_KEY);

  const followSystem = ref(storedTheme === null);
  const theme = ref<ThemeName>(
    storedTheme !== null && isThemeName(storedTheme)
      ? storedTheme
      : systemTheme(dark.matches),
  );
  const density = ref<DensityName>(
    storedDensity !== null && isDensityName(storedDensity) ? storedDensity : "compact",
  );
  const revision = ref(0);

  function systemTheme(prefersDark: boolean): ThemeName {
    const match = Object.values(themes).find(
      (t) => t.scheme === (prefersDark ? "dark" : "light"),
    );
    return match?.name ?? DEFAULT_THEME;
  }

  function apply() {
    env.root.dataset.theme = theme.value;
    env.root.dataset.density = density.value;
    // The installed PWA's title bar and the Android address bar read this;
    // without it the window chrome stays the color of whichever theme was
    // compiled into `index.html`.
    const meta = env.document.querySelector<HTMLMetaElement>(
      'meta[name="theme-color"]:not([media])',
    );
    const color = themes[theme.value].browserColor;
    if (meta) {
      meta.content = color;
    } else {
      const added = env.document.createElement("meta");
      added.name = "theme-color";
      added.content = color;
      env.document.head.appendChild(added);
    }
    revision.value += 1;
  }

  dark.addEventListener("change", (e) => {
    if (!followSystem.value) return;
    theme.value = systemTheme(e.matches);
    apply();
  });

  apply();

  return {
    theme,
    density,
    current: computed(() => themes[theme.value]),
    followSystem,
    setTheme(name) {
      theme.value = name;
      followSystem.value = false;
      env.storage.write(THEME_KEY, name);
      apply();
    },
    setDensity(name) {
      density.value = name;
      env.storage.write(DENSITY_KEY, name);
      apply();
    },
    revision,
  };
}

/**
 * The resolved value of one token, as the browser sees it.
 *
 * Canvas and WebGPU surfaces must not import a theme object: an operator
 * override in the working directory, or a future user token file, lands in
 * CSS, and a timeline that read the TypeScript value would be the one surface
 * that ignored it. So read the computed custom property instead.
 */
export function resolveToken(el: Element, path: string): string {
  return getComputedStyle(el).getPropertyValue(varName(path)).trim();
}

/** Every color token, flattened, as the browser currently resolves it. */
export function resolvePalette(el: Element, palette: Palette): Record<string, string> {
  const style = getComputedStyle(el);
  const out: Record<string, string> = {};
  for (const path of Object.keys(flatten(palette, ["color"]))) {
    out[path] = style.getPropertyValue(varName(path)).trim();
  }
  return out;
}

/**
 * A snapshot of the palette for a canvas surface, refreshed whenever the
 * theme or density changes.
 *
 * `shallowRef` and an explicit refresh, rather than a computed over
 * `getComputedStyle`: resolving a hundred properties on every frame of a
 * timeline redraw is exactly the kind of cost that makes canvas surfaces feel
 * unlike the DOM around them.
 */
export function useCanvasTokens(el: Ref<Element | null>, controller: ThemeController) {
  const snapshot = shallowRef<Record<string, string>>({});

  function refresh() {
    const node = el.value;
    if (!node) return;
    snapshot.value = resolvePalette(node, controller.current.value.color);
  }

  return {
    snapshot,
    refresh,
    /** `token("color-accent")`, with the theme's own value as the fallback. */
    token(path: string): string {
      return snapshot.value[path] ?? "";
    },
  };
}
