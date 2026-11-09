import type { Theme } from "../types.ts";

/**
 * Circuit: the daylight set. A bare-board ground, a trace-green accent and
 * copper for anything that needs attention.
 *
 * It exists because a laptop in sunlight is a real case and inverting Night
 * is not a theme. Keeping one light and one dark set from the first commit
 * also forces the token names to stay semantic: anything called
 * `--dj-color-gray-700` would have had to be renamed here.
 *
 * TODO: agentmancer's original Circuit values were not available when this
 * was written (its repo is not checked out alongside Deckjack). These are
 * built from the same hues as Night rather than copied, so if the original
 * matters, replace this file — nothing else needs to change.
 */
export const circuit: Theme = {
  name: "circuit",
  label: "Circuit",
  scheme: "light",
  // Matches `color.raised`, which is what the top bar and the dock
  // paint with, so the browser chrome continues the shell rather than
  // framing it.
  browserColor: "#FFFFFF",
  color: {
    ground: "#F3F1EA",
    groundPattern: "rgba(14, 124, 107, 0.07)",
    surface: "#FBFAF6",
    raised: "#FFFFFF",
    sunken: "#EEEBE1",
    overlay: "#FFFFFF",
    scrim: "rgba(23, 26, 22, 0.32)",

    hairline: "#E4E1D6",
    border: "#CFCBBD",
    borderStrong: "#A9A596",
    focusRing: "#0E7C6B",

    text: "#171A16",
    textMuted: "#565C53",
    textDim: "#7C8378",
    textOnAccent: "#FFFFFF",

    accent: "#0E7C6B",
    accentHover: "#0A6356",
    accentSoft: "rgba(14, 124, 107, 0.12)",
    accentLine: "rgba(14, 124, 107, 0.35)",

    ok: "#137A4E",
    okSoft: "rgba(19, 122, 78, 0.12)",
    warn: "#8A5A00",
    warnSoft: "rgba(138, 90, 0, 0.12)",
    danger: "#B3261E",
    dangerSoft: "rgba(179, 38, 30, 0.10)",
    info: "#1B5FB0",
    infoSoft: "rgba(27, 95, 176, 0.12)",

    textOnWarn: "#FFFFFF",
    textOnDanger: "#FFFFFF",

    selection: "rgba(14, 124, 107, 0.20)",
    skeleton: "#E7E4DA",
    skeletonSheen: "rgba(255, 255, 255, 0.72)",

    risk: {
      read: "#565C53",
      write: "#1B5FB0",
      destructive: "#B3261E",
    },

    shadow: {
      low: "0 1px 2px rgba(23, 26, 22, 0.10)",
      mid: "0 6px 18px rgba(23, 26, 22, 0.12)",
      high: "0 16px 40px rgba(23, 26, 22, 0.16)",
      inset: "inset 0 1px 0 rgba(255, 255, 255, 0.70)",
    },

    terminal: {
      // A light theme gets a light terminal. A black rectangle on a paper
      // desktop is the hole in the desktop this system exists to avoid.
      background: "#FDFCF9",
      foreground: "#171A16",
      cursor: "#0E7C6B",
      cursorText: "#FFFFFF",
      selection: "rgba(14, 124, 107, 0.20)",
      selectionText: "#171A16",
      ansi: {
        black: "#171A16",
        red: "#B3261E",
        green: "#137A4E",
        yellow: "#8A5A00",
        blue: "#1B5FB0",
        magenta: "#7A3EA8",
        cyan: "#0E7C6B",
        white: "#565C53",
        brightBlack: "#7C8378",
        brightRed: "#D23A2F",
        brightGreen: "#1C9A64",
        brightYellow: "#A97300",
        brightBlue: "#2A79D6",
        brightMagenta: "#9455C6",
        brightCyan: "#159C87",
        brightWhite: "#171A16",
      },
    },

    syntax: {
      comment: "#7C8378",
      keyword: "#7A3EA8",
      control: "#B3261E",
      operator: "#565C53",
      punctuation: "#565C53",
      string: "#137A4E",
      escape: "#1C9A64",
      number: "#8A5A00",
      constant: "#A97300",
      variable: "#171A16",
      property: "#1B5FB0",
      parameter: "#565C53",
      function: "#1B5FB0",
      type: "#0E7C6B",
      tag: "#7A3EA8",
      attribute: "#159C87",
      invalid: "#B3261E",
      diffAdded: "#137A4E",
      diffRemoved: "#B3261E",
      diffChanged: "#8A5A00",
    },

    canvas: {
      grid: "rgba(23, 26, 22, 0.10)",
      gridStrong: "rgba(23, 26, 22, 0.20)",
      axis: "#A9A596",
      playhead: "#B3261E",
      waveform: "rgba(14, 124, 107, 0.55)",
      waveformPeak: "#0A6356",
      clipVideo: "#DCE7EA",
      clipAudio: "#DCEAE2",
      clipSelected: "#0E7C6B",
      scopeTrace: "#1B5FB0",
      scopeClip: "#B3261E",
    },

    series: [
      "#0E7C6B",
      "#1B5FB0",
      "#8A5A00",
      "#7A3EA8",
      "#137A4E",
      "#B3261E",
      "#159C87",
      "#565C53",
    ],
  },
};
