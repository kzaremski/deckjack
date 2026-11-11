import type { Component } from "vue";

import { isInstalled, type AppId } from "../../stores/apps.ts";
import AppFramePanel from "./AppFramePanel.vue";
import PlaceholderPanel from "./PlaceholderPanel.vue";
import TerminalPanel from "./TerminalPanel.vue";

/**
 * Which component renders which app.
 *
 * Panels land one at a time behind their backends, and each one is a single
 * line here — nothing else in the shell has to learn about a new app. The
 * terminal is first because it is the Phase 1 exit criterion; the rest are
 * still placeholders.
 */
export const panels: Partial<Record<AppId, Component>> = {
  terminal: TerminalPanel,
};

/**
 * Which component renders one window.
 *
 * Three answers, in order. A built-in panel, for the parts of the desktop the
 * core itself provides — the terminal is one, because the pty is the core's.
 * Otherwise an installed app binary, whose UI the core serves at
 * `/apps/<name>/` and which needs nothing in this bundle at all. Otherwise
 * the placeholder, which is what an app named in the built-in table but not
 * installed honestly is.
 *
 * `appId` and not `app`: the first is what the server called it, and an
 * installed app is not one of the shell's own `AppId`s.
 */
export function panelFor(app: AppId, appId?: string): Component {
  const builtin = panels[app];
  if (builtin) return builtin;
  if (appId && isInstalled(appId)) return AppFramePanel;
  return PlaceholderPanel;
}

export { AppFramePanel, PlaceholderPanel };
