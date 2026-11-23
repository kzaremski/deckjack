import type { Component } from "vue";

import { isNativeAppId } from "../../shell/stream.ts";
import type { AppId, AppTable } from "../../stores/apps.ts";
import UiProtocolPanel from "../../uiproto/UiProtocolPanel.vue";
import AppFramePanel from "./AppFramePanel.vue";
import PlaceholderPanel from "./PlaceholderPanel.vue";
import StreamPanel from "./StreamPanel.vue";
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
 * Five answers, in order. A streamed native app's window (`native:<name>`),
 * which the stream panel draws from the node's H.264. A built-in panel, for
 * the parts of the desktop the core itself provides — the terminal is one,
 * because the pty is the core's. Otherwise an installed app that describes
 * its window over the UI protocol (`tier = 1`), drawn by the shell's own
 * components with no app code at all. Otherwise an installed app binary
 * that serves its own page, which the core serves at `/apps/<name>/` and
 * this bundle frames. Otherwise the placeholder, which is what an app named
 * in the built-in table but not installed honestly is.
 *
 * `apps` is the shell's app table, passed in rather than imported, because
 * the table belongs to one shell and not to the module.
 */
export function panelFor(app: AppId, apps: Pick<AppTable, "isInstalled" | "isDescribed">): Component {
  if (isNativeAppId(app)) return StreamPanel;
  const builtin = panels[app];
  if (builtin) return builtin;
  if (apps.isDescribed(app)) return UiProtocolPanel;
  if (apps.isInstalled(app)) return AppFramePanel;
  return PlaceholderPanel;
}

export { AppFramePanel, PlaceholderPanel, StreamPanel, UiProtocolPanel };
