import type { Component } from "vue";

import type { AppId } from "../../stores/apps.ts";
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

export function panelFor(app: AppId): Component {
  return panels[app] ?? PlaceholderPanel;
}

export { PlaceholderPanel };
