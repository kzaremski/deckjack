import type { Theme } from "../types.ts";

/**
 * Night: the default. A deep blue-black substrate with one teal accent.
 *
 * These are the values from the shell prototype, kept verbatim so the built
 * shell and the design canvas do not drift apart. The accent is the only
 * saturated color in the set, which is what lets it mean "focused", "live"
 * and "primary action" without competing with anything.
 *
 * Status colors are deliberately not the accent: a green that also means
 * "this window has focus" is a green that cannot mean "the build passed".
 */
export const night: Theme = {
  name: "night",
  label: "Night",
  scheme: "dark",
  // Matches `color.raised`, which is what the top bar and the dock
  // paint with, so the browser chrome continues the shell rather than
  // framing it.
  browserColor: "#161D27",
  color: {
    ground: "#0A0D11",
    groundPattern: "rgba(47, 211, 192, 0.05)",
    surface: "#0F141A",
    raised: "#161D27",
    sunken: "#0A0D11",
    overlay: "#11161D",
    scrim: "rgba(5, 7, 10, 0.55)",

    hairline: "#1A212B",
    border: "#263040",
    borderStrong: "#2F3B4D",
    focusRing: "#2FD3C0",

    text: "#E7ECF3",
    textMuted: "#93A0B2",
    textDim: "#5E6A7A",
    textOnAccent: "#06201D",

    accent: "#2FD3C0",
    accentHover: "#7BE8DB",
    accentSoft: "rgba(47, 211, 192, 0.16)",
    accentLine: "rgba(47, 211, 192, 0.40)",

    ok: "#4ED08A",
    okSoft: "rgba(78, 208, 138, 0.16)",
    warn: "#F5B544",
    warnSoft: "rgba(245, 181, 68, 0.16)",
    danger: "#FF6B5A",
    dangerSoft: "rgba(255, 107, 90, 0.16)",
    info: "#6AA9FF",
    infoSoft: "rgba(106, 169, 255, 0.16)",

    textOnWarn: "#2A1C00",
    textOnDanger: "#2A0906",

    selection: "rgba(47, 211, 192, 0.28)",
    skeleton: "#1A2230",
    skeletonSheen: "rgba(231, 236, 243, 0.06)",

    risk: {
      read: "#93A0B2",
      write: "#6AA9FF",
      destructive: "#FF6B5A",
    },

    shadow: {
      low: "0 1px 2px rgba(0, 0, 0, 0.40)",
      mid: "0 8px 24px rgba(0, 0, 0, 0.42)",
      high: "0 18px 48px rgba(0, 0, 0, 0.45)",
      inset: "inset 0 1px 0 rgba(231, 236, 243, 0.04)",
    },

    terminal: {
      // A shade below `surface`: the terminal reads as a well inside its
      // window rather than as a separate application.
      background: "#080B0F",
      foreground: "#E7ECF3",
      cursor: "#2FD3C0",
      cursorText: "#06201D",
      selection: "rgba(47, 211, 192, 0.28)",
      selectionText: "#E7ECF3",
      ansi: {
        black: "#11161D",
        red: "#FF6B5A",
        green: "#4ED08A",
        yellow: "#F5B544",
        blue: "#6AA9FF",
        magenta: "#C98BFF",
        cyan: "#2FD3C0",
        white: "#C9D2DE",
        brightBlack: "#5E6A7A",
        brightRed: "#FF9387",
        brightGreen: "#7FE0AC",
        brightYellow: "#FFD083",
        brightBlue: "#9CC5FF",
        brightMagenta: "#DCB2FF",
        brightCyan: "#7BE8DB",
        brightWhite: "#F4F7FB",
      },
    },

    syntax: {
      comment: "#5E6A7A",
      keyword: "#C98BFF",
      control: "#FF9387",
      operator: "#93A0B2",
      punctuation: "#93A0B2",
      string: "#4ED08A",
      escape: "#7FE0AC",
      number: "#F5B544",
      constant: "#FFD083",
      variable: "#E7ECF3",
      property: "#9CC5FF",
      parameter: "#C9D2DE",
      function: "#6AA9FF",
      type: "#2FD3C0",
      tag: "#C98BFF",
      attribute: "#7BE8DB",
      invalid: "#FF6B5A",
      diffAdded: "#4ED08A",
      diffRemoved: "#FF6B5A",
      diffChanged: "#F5B544",
    },

    canvas: {
      grid: "rgba(147, 160, 178, 0.14)",
      gridStrong: "rgba(147, 160, 178, 0.28)",
      axis: "#2F3B4D",
      playhead: "#FF6B5A",
      waveform: "rgba(47, 211, 192, 0.55)",
      waveformPeak: "#7BE8DB",
      clipVideo: "#1F3A4A",
      clipAudio: "#1F3A33",
      clipSelected: "#2FD3C0",
      scopeTrace: "#9CC5FF",
      scopeClip: "#FF6B5A",
    },

    series: [
      "#2FD3C0",
      "#6AA9FF",
      "#F5B544",
      "#C98BFF",
      "#4ED08A",
      "#FF9387",
      "#7BE8DB",
      "#C9D2DE",
    ],
  },
};
