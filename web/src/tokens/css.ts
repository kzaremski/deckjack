import { foundation } from "./foundation.ts";
import { themes } from "./registry.ts";
import type { DensityName, Palette, Theme } from "./types.ts";

/**
 * Turns the token data into CSS custom properties.
 *
 * This is a pure string builder on purpose: the shell runs it in the browser
 * to install one stylesheet, and `scripts/emit-tokens.ts` runs the same
 * functions under Node to write the file the xterm, Monaco, mermaid and
 * code-server generators read. Two code paths producing token values would
 * eventually produce two different token values.
 */

/** Every custom property is namespaced, so an embedded third-party UI's own
 * variables can never collide with the shell's. */
export const PREFIX = "--dj";

function kebab(key: string): string {
  return key.replace(/([a-z0-9])([A-Z])/g, "$1-$2").toLowerCase();
}

/**
 * Flattens a nested token object to `path -> value`, joining with `-`:
 * `terminal.ansi.brightBlack` becomes `terminal-ansi-bright-black`. Arrays
 * are indexed from 1, because `series-1` reads better than `series-0` in a
 * stylesheet and generators treat the list as ordered either way.
 */
export function flatten(
  value: unknown,
  path: string[] = [],
  out: Record<string, string> = {},
): Record<string, string> {
  if (Array.isArray(value)) {
    value.forEach((v, i) => flatten(v, [...path, String(i + 1)], out));
    return out;
  }
  if (value !== null && typeof value === "object") {
    for (const [k, v] of Object.entries(value)) {
      flatten(v, [...path, kebab(k)], out);
    }
    return out;
  }
  out[path.join("-")] = String(value);
  return out;
}

/** `color.accent` -> `--dj-color-accent`. */
export function varName(path: string): string {
  return `${PREFIX}-${path}`;
}

/** `color.accent` -> `var(--dj-color-accent)`, for use in component styles. */
export function cssVar(path: string, fallback?: string): string {
  return fallback === undefined
    ? `var(${varName(path)})`
    : `var(${varName(path)}, ${fallback})`;
}

export function paletteVariables(color: Palette): Record<string, string> {
  const flat = flatten(color, ["color"]);
  const out: Record<string, string> = {};
  for (const [k, v] of Object.entries(flat)) out[varName(k)] = v;
  return out;
}

function block(selector: string, vars: Record<string, string>): string {
  const body = Object.entries(vars)
    .map(([k, v]) => `  ${k}: ${v};`)
    .join("\n");
  return `${selector} {\n${body}\n}`;
}

/**
 * Theme and density are attributes on `<html>` rather than classes, so a
 * single attribute selector can switch every surface and a canvas surface can
 * read the current theme name back without guessing.
 */
export function themeCss(theme: Theme): string {
  const vars = paletteVariables(theme.color);
  // Lets the browser style scrollbars, form controls and the caret to match,
  // which is the difference between a themed app and a themed-looking app.
  vars["color-scheme"] = theme.scheme;
  return block(`:root[data-theme="${theme.name}"]`, vars);
}

export function densityCss(name: DensityName): string {
  const d = foundation.density[name];
  return block(`:root[data-density="${name}"]`, {
    [varName("unit")]: d.unit,
    [varName("control")]: d.control,
    [varName("touch")]: d.touch,
    [varName("row")]: d.row,
    [varName("titlebar")]: d.titlebar,
    [varName("font-scale")]: String(d.fontScale),
  });
}

/**
 * The density-independent half: fonts, the type scale, spacing multiples,
 * radii, motion and the stacking order.
 *
 * Type sizes are multiplied by the density font scale here rather than in
 * each density block, so adding a type step cannot forget a density.
 */
export function foundationCss(): string {
  const vars: Record<string, string> = {};

  vars[varName("font-sans")] = foundation.font.sans;
  vars[varName("font-mono")] = foundation.font.mono;
  vars[varName("font-display")] = foundation.font.display;
  vars[varName("font-mono-features")] = foundation.font.monoFeatures;

  for (const [step, t] of Object.entries(foundation.type)) {
    const s = kebab(step);
    vars[varName(`type-${s}-size`)] = `calc(${t.size} * var(${varName("font-scale")}, 1))`;
    vars[varName(`type-${s}-line`)] = t.line;
    vars[varName(`type-${s}-tracking`)] = t.tracking;
    vars[varName(`type-${s}-weight`)] = String(t.weight);
  }

  for (const n of Object.keys(foundation.space)) {
    vars[varName(`space-${n}`)] = `calc(var(${varName("unit")}, 4px) * ${n})`;
  }

  for (const [step, r] of Object.entries(foundation.radius)) {
    vars[varName(`radius-${step}`)] = r;
  }

  for (const [step, w] of Object.entries(foundation.stroke)) {
    vars[varName(`stroke-${step}`)] = w;
  }

  for (const [step, d] of Object.entries(foundation.motion.duration)) {
    vars[varName(`duration-${step}`)] = d;
  }
  for (const [step, e] of Object.entries(foundation.motion.easing)) {
    vars[varName(`ease-${step}`)] = e;
  }

  for (const [step, z] of Object.entries(foundation.layer)) {
    vars[varName(`layer-${kebab(step)}`)] = String(z);
  }

  for (const [name, px] of Object.entries(foundation.breakpoint)) {
    vars[varName(`breakpoint-${kebab(name)}`)] = `${px}px`;
  }

  return block(":root", vars);
}

/** The whole stylesheet: foundation, every density, every theme. */
export function tokenStylesheet(): string {
  const parts = [foundationCss()];
  for (const name of Object.keys(foundation.density) as DensityName[]) {
    parts.push(densityCss(name));
  }
  for (const theme of Object.values(themes)) {
    parts.push(themeCss(theme));
  }
  return parts.join("\n\n") + "\n";
}
