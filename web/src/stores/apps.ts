import { ref } from "vue";

import type { Size } from "../shell/geometry.ts";

/**
 * The app contract.
 *
 * Every app declares a compact layout, a wide layout and a minimum width, and
 * responds to whatever space it is given. The window manager needs those three
 * facts before an app's backend exists, so they live here as data rather than
 * inside each panel.
 *
 * This is the shell's view of an app, not the app's manifest. On a real
 * install the core scans `~/.deckjack/apps/<name>/app.toml` and sends the list
 * down the bus; this table is the built-in set plus a fallback, so the shell
 * is usable before any app binary is installed.
 */

export type AppId =
  | "files"
  | "terminal"
  | "notes"
  | "tasks"
  | "mail"
  | "calendar"
  | "assistant";

export interface AppContract {
  id: AppId;
  title: string;
  /** Two characters for the window tag and the dock. Short enough to read at
   * 22px, which an icon font would not be at compact density. */
  tag: string;
  summary: string;
  minSize: Size;
  defaultSize: Size;
  /** False when the app refuses to render in that layout at all. */
  compact: boolean;
  wide: boolean;
  /** Shows the Inbox count on the dock, per "pending approvals surface as a
   * dock badge and a notification". */
  badge?: "inbox";
}

export const apps: Record<AppId, AppContract> = {
  files: {
    id: "files",
    title: "Files",
    tag: "Fi",
    summary: "The working directory, across nodes",
    minSize: { w: 360, h: 220 },
    defaultSize: { w: 680, h: 440 },
    compact: true,
    wide: true,
  },
  terminal: {
    id: "terminal",
    title: "Terminal",
    tag: "Te",
    // A terminal narrower than 80 columns is a different tool, so the
    // minimum is a real constraint rather than a guess.
    summary: "A real shell on the host, over a PTY",
    minSize: { w: 420, h: 200 },
    defaultSize: { w: 640, h: 400 },
    compact: true,
    wide: true,
  },
  notes: {
    id: "notes",
    title: "Notes",
    tag: "No",
    summary: "Documents in the working directory",
    minSize: { w: 340, h: 220 },
    defaultSize: { w: 620, h: 480 },
    compact: true,
    wide: true,
  },
  tasks: {
    id: "tasks",
    title: "Tasks",
    tag: "Ta",
    summary: "Task lists and reminders",
    minSize: { w: 320, h: 220 },
    defaultSize: { w: 480, h: 520 },
    compact: true,
    wide: true,
  },
  mail: {
    id: "mail",
    title: "Mail",
    tag: "Ma",
    summary: "The local mirror of the mailbox",
    minSize: { w: 360, h: 240 },
    defaultSize: { w: 760, h: 520 },
    compact: true,
    wide: true,
  },
  calendar: {
    id: "calendar",
    title: "Calendar",
    tag: "Ca",
    summary: "Events and alarms, as iCalendar",
    minSize: { w: 360, h: 260 },
    defaultSize: { w: 820, h: 540 },
    compact: true,
    wide: true,
  },
  assistant: {
    id: "assistant",
    title: "Assistant",
    tag: "As",
    summary: "Agent sessions and the Inbox of pending approvals",
    minSize: { w: 340, h: 320 },
    defaultSize: { w: 420, h: 540 },
    compact: true,
    wide: true,
    badge: "inbox",
  },
};

/** Dock order. The Assistant sits last: it is one app among many. */
export const dockOrder: AppId[] = [
  "files",
  "terminal",
  "notes",
  "tasks",
  "mail",
  "calendar",
  "assistant",
];

export function isAppId(value: string): value is AppId {
  return value in apps;
}

/** For a window whose app the shell does not know: an installed app binary
 * the core told us about, or one that was uninstalled while open. */
export function unknownApp(id: string): AppContract {
  return {
    id: "files",
    title: id,
    tag: id.slice(0, 2).padEnd(2, " "),
    summary: "An installed app this shell has no built-in panel for",
    minSize: { w: 320, h: 200 },
    defaultSize: { w: 640, h: 420 },
    compact: true,
    wide: true,
  };
}

/**
 * Apps the core has discovered on this node, by name.
 *
 * The built-in table above is what the shell knows before any app binary is
 * installed; this is what is actually there. `system.apps` reports it, and a
 * window for one of these renders the app's own UI in a frame rather than a
 * built-in panel.
 *
 * A module-level ref rather than part of the windows store: the dock, the
 * palette and the window surface all want the same answer, and none of them
 * owns it.
 */
export interface InstalledApp {
  name: string;
  title: string;
  version: string;
  lifecycle: "service" | "on-demand";
  minWidth?: number;
  status: "live" | "reconnecting" | "unavailable";
}

export const installed = ref<InstalledApp[]>([]);

export function isInstalled(name: string): boolean {
  return installed.value.some((app) => app.name === name);
}

/**
 * Ask the core what is installed.
 *
 * Failure is not an error worth surfacing: `system.apps` is absent on a core
 * with no app host, and the honest result then is "no installed apps" rather
 * than a banner about a command the operator cannot do anything about.
 */
export async function loadInstalled(
  call: (command: string, params?: Record<string, unknown>) => Promise<unknown>,
): Promise<void> {
  try {
    const result = (await call("system.apps")) as { apps?: unknown } | undefined;
    const list = Array.isArray(result?.apps) ? result.apps : [];
    installed.value = list
      .filter((a): a is Record<string, unknown> => typeof a === "object" && a !== null)
      .map<InstalledApp>((a) => ({
        name: String(a.name ?? ""),
        title: String(a.title ?? a.name ?? ""),
        version: String(a.version ?? ""),
        lifecycle: a.lifecycle === "service" ? "service" : "on-demand",
        minWidth: typeof a.min_width === "number" ? a.min_width : undefined,
        status:
          a.status === "live" || a.status === "reconnecting" ? a.status : "unavailable",
      }))
      .filter((a) => a.name !== "");
  } catch {
    installed.value = [];
  }
}
