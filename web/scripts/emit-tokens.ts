/**
 * Emits the token set for every target that is not the DOM.
 *
 * The design rule is that one token file styles every surface, including ones
 * owned by programs that have never heard of Deckjack. That only works if
 * their themes are generated from these tokens rather than written by hand
 * next to them, so this script is the single place where a non-DOM target is
 * allowed to learn a color.
 *
 * Run with `npm run tokens`. It needs Node 22.6 or newer, which strips the
 * types from these files directly; there is no build step and no extra
 * dependency, so `cargo build` on a machine with Node can call it.
 *
 * Adding a target means adding a function here. Never a second palette.
 */

import { mkdirSync, writeFileSync } from "node:fs";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";

import { tokenStylesheet } from "../src/tokens/css.ts";
import { themeList, tokenManifest } from "../src/tokens/registry.ts";
import type { Theme } from "../src/tokens/types.ts";

const here = dirname(fileURLToPath(import.meta.url));
const outDir = resolve(here, "..", "generated");

/** xterm.js `ITheme`. The terminal is a surface, not an application. */
function xtermTheme(theme: Theme) {
  const c = theme.color.terminal;
  return {
    background: c.background,
    foreground: c.foreground,
    cursor: c.cursor,
    cursorAccent: c.cursorText,
    selectionBackground: c.selection,
    selectionForeground: c.selectionText,
    black: c.ansi.black,
    red: c.ansi.red,
    green: c.ansi.green,
    yellow: c.ansi.yellow,
    blue: c.ansi.blue,
    magenta: c.ansi.magenta,
    cyan: c.ansi.cyan,
    white: c.ansi.white,
    brightBlack: c.ansi.brightBlack,
    brightRed: c.ansi.brightRed,
    brightGreen: c.ansi.brightGreen,
    brightYellow: c.ansi.brightYellow,
    brightBlue: c.ansi.brightBlue,
    brightMagenta: c.ansi.brightMagenta,
    brightCyan: c.ansi.brightCyan,
    brightWhite: c.ansi.brightWhite,
  };
}

/**
 * A VS Code color theme, which is what code-server loads and what Monaco's
 * `defineTheme` consumes after conversion. Only the colors an embedded editor
 * actually shows are set: leaving the rest to VS Code's own default for the
 * declared `type` is more robust across versions than guessing at keys that
 * come and go.
 */
function vsCodeTheme(theme: Theme) {
  const c = theme.color;
  const s = c.syntax;
  const scope = (scopes: string[], color: string) => ({
    scope: scopes,
    settings: { foreground: color },
  });
  return {
    name: `Deckjack ${theme.label}`,
    type: theme.scheme,
    colors: {
      "editor.background": c.surface,
      "editor.foreground": c.text,
      "editorCursor.foreground": c.accent,
      "editor.selectionBackground": c.selection,
      "editor.lineHighlightBackground": c.raised,
      "editorLineNumber.foreground": c.textDim,
      "editorLineNumber.activeForeground": c.text,
      "editorIndentGuide.background1": c.hairline,
      "editorWhitespace.foreground": c.hairline,
      "editorGutter.addedBackground": s.diffAdded,
      "editorGutter.deletedBackground": s.diffRemoved,
      "editorGutter.modifiedBackground": s.diffChanged,
      "editorWidget.background": c.overlay,
      "editorWidget.border": c.border,
      "editorHoverWidget.background": c.overlay,
      "editorError.foreground": c.danger,
      "editorWarning.foreground": c.warn,
      "editorInfo.foreground": c.info,
      "sideBar.background": c.surface,
      "sideBar.foreground": c.textMuted,
      "sideBarSectionHeader.background": c.raised,
      "activityBar.background": c.raised,
      "activityBar.foreground": c.text,
      "activityBarBadge.background": c.accent,
      "activityBarBadge.foreground": c.textOnAccent,
      "statusBar.background": c.raised,
      "statusBar.foreground": c.textMuted,
      "statusBar.border": c.border,
      "titleBar.activeBackground": c.raised,
      "titleBar.activeForeground": c.text,
      "tab.activeBackground": c.surface,
      "tab.inactiveBackground": c.raised,
      "tab.activeForeground": c.text,
      "tab.inactiveForeground": c.textMuted,
      "tab.border": c.hairline,
      "tab.activeBorderTop": c.accent,
      "panel.background": c.surface,
      "panel.border": c.border,
      "terminal.background": c.terminal.background,
      "terminal.foreground": c.terminal.foreground,
      "terminalCursor.foreground": c.terminal.cursor,
      "terminal.ansiBlack": c.terminal.ansi.black,
      "terminal.ansiRed": c.terminal.ansi.red,
      "terminal.ansiGreen": c.terminal.ansi.green,
      "terminal.ansiYellow": c.terminal.ansi.yellow,
      "terminal.ansiBlue": c.terminal.ansi.blue,
      "terminal.ansiMagenta": c.terminal.ansi.magenta,
      "terminal.ansiCyan": c.terminal.ansi.cyan,
      "terminal.ansiWhite": c.terminal.ansi.white,
      "terminal.ansiBrightBlack": c.terminal.ansi.brightBlack,
      "terminal.ansiBrightRed": c.terminal.ansi.brightRed,
      "terminal.ansiBrightGreen": c.terminal.ansi.brightGreen,
      "terminal.ansiBrightYellow": c.terminal.ansi.brightYellow,
      "terminal.ansiBrightBlue": c.terminal.ansi.brightBlue,
      "terminal.ansiBrightMagenta": c.terminal.ansi.brightMagenta,
      "terminal.ansiBrightCyan": c.terminal.ansi.brightCyan,
      "terminal.ansiBrightWhite": c.terminal.ansi.brightWhite,
      "focusBorder": c.focusRing,
      "foreground": c.text,
      "widget.shadow": "#00000000",
      "scrollbarSlider.background": c.border,
      "scrollbarSlider.hoverBackground": c.borderStrong,
      "input.background": c.sunken,
      "input.foreground": c.text,
      "input.border": c.border,
      "list.activeSelectionBackground": c.accentSoft,
      "list.activeSelectionForeground": c.text,
      "list.hoverBackground": c.raised,
      "button.background": c.accent,
      "button.foreground": c.textOnAccent,
      "badge.background": c.accent,
      "badge.foreground": c.textOnAccent,
    },
    tokenColors: [
      {
        scope: ["comment", "punctuation.definition.comment"],
        settings: { foreground: s.comment, fontStyle: "italic" },
      },
      scope(["keyword", "storage", "storage.type", "storage.modifier"], s.keyword),
      scope(["keyword.control", "keyword.operator.new"], s.control),
      scope(["keyword.operator"], s.operator),
      scope(["punctuation", "meta.brace"], s.punctuation),
      scope(["string", "string.quoted"], s.string),
      scope(["constant.character.escape"], s.escape),
      scope(["constant.numeric"], s.number),
      scope(["constant.language", "constant.other"], s.constant),
      scope(["variable", "variable.other"], s.variable),
      scope(["variable.other.property", "meta.object-literal.key"], s.property),
      scope(["variable.parameter"], s.parameter),
      scope(["entity.name.function", "support.function"], s.function),
      scope(["entity.name.type", "support.type", "support.class"], s.type),
      scope(["entity.name.tag"], s.tag),
      scope(["entity.other.attribute-name"], s.attribute),
      {
        scope: ["invalid", "invalid.illegal"],
        settings: { foreground: s.invalid, fontStyle: "underline" },
      },
      scope(["markup.inserted"], s.diffAdded),
      scope(["markup.deleted"], s.diffRemoved),
      scope(["markup.changed"], s.diffChanged),
    ],
  };
}

/**
 * Mermaid's theme variables, for diagrams rendered inside notes and docs. A
 * diagram is content, so it uses the series colors the rest of the shell uses
 * for categorical data rather than mermaid's pastels.
 */
function mermaidTheme(theme: Theme) {
  const c = theme.color;
  const series = (i: number) => c.series[i % c.series.length] ?? c.accent;
  return {
    theme: "base",
    themeVariables: {
      darkMode: theme.scheme === "dark",
      background: c.surface,
      fontFamily: "var(--dj-font-sans)",
      fontSize: "14px",
      primaryColor: c.accentSoft,
      primaryTextColor: c.text,
      primaryBorderColor: c.accentLine,
      secondaryColor: c.raised,
      secondaryTextColor: c.text,
      secondaryBorderColor: c.border,
      tertiaryColor: c.sunken,
      tertiaryTextColor: c.textMuted,
      tertiaryBorderColor: c.hairline,
      lineColor: c.borderStrong,
      textColor: c.text,
      mainBkg: c.raised,
      nodeBorder: c.border,
      clusterBkg: c.surface,
      clusterBorder: c.hairline,
      noteBkgColor: c.warnSoft,
      noteTextColor: c.text,
      noteBorderColor: c.warn,
      errorBkgColor: c.dangerSoft,
      errorTextColor: c.danger,
      cScale0: series(0),
      cScale1: series(1),
      cScale2: series(2),
      cScale3: series(3),
      cScale4: series(4),
      cScale5: series(5),
    },
  };
}

function write(relative: string, body: string) {
  const path = join(outDir, relative);
  mkdirSync(dirname(path), { recursive: true });
  writeFileSync(path, body.endsWith("\n") ? body : `${body}\n`, "utf8");
  console.log(`wrote generated/${relative}`);
}

function json(value: unknown): string {
  return JSON.stringify(value, null, 2);
}

// The manifest is what the Rust side embeds: canvas and WebGPU surfaces on
// the server, and anything that has to know a token value without a browser.
write("tokens.json", json(tokenManifest));
write("tokens.css", tokenStylesheet());

for (const theme of themeList) {
  write(`xterm/${theme.name}.json`, json(xtermTheme(theme)));
  write(`vscode/deckjack-${theme.name}-color-theme.json`, json(vsCodeTheme(theme)));
  write(`mermaid/${theme.name}.json`, json(mermaidTheme(theme)));
}

console.log(`${themeList.length} themes emitted to ${outDir}`);
