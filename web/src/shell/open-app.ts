import type { AppTable } from "../stores/apps.ts";

/**
 * Opening an app on a file: the shell's half of the open-app contract.
 *
 * An app's frame calls `shell.openApp(name, { path })` from `/kit/shell.js`,
 * which posts `{ type: "shell.app.open", app, path }` to the window hosting
 * it. The frame bridge checks where the message came from; this module checks
 * what it says, for the bridge and for the `shell.app.open` command alike, so
 * a palette entry or an agent asking the same thing meets the same rules.
 *
 * The window that answers gets the path as `?path=` on its frame URL when it
 * is new, and as a `deckjack.app.open` message when it was already open, so a
 * file opened into a running editor does not reload it and lose the unsaved
 * text in its other tabs.
 */

/** The message type a frame posts, and the shell command it becomes. */
export const APP_OPEN = "shell.app.open";

/** The message the shell posts into an already-open app frame. */
export const APP_LAUNCH_MESSAGE = "deckjack.app.open";

/** A frame asking whether an app can be opened, without opening it. */
export const APP_CAN_OPEN = "shell.app.can-open";

/**
 * The shell's answer to a frame request that carried an `id`:
 * `{ type, id, ok, reason? }`, posted back to that frame.
 */
export const FRAME_REPLY = "deckjack.shell.reply";

/** Longest path accepted: a node name, a colon and the core's own limit. */
const MAX_PATH = 4096 + 64;

export interface AppOpenRequest {
  app: string;
  /** A working-directory address: `/a/b`, `a/b` (made `/a/b`) or `node:/a/b`. */
  path?: string;
}

export type Parsed = { ok: true; request: AppOpenRequest } | { ok: false; reason: string };

/**
 * Validate a request to open an app, from a frame, the palette or an agent.
 *
 * The app must be one the app table knows, so a frame cannot open a window
 * for a name nothing installed. The path is checked only for shape: the core
 * resolves it, and the core is what refuses `.deckjack/` and anything outside
 * the working directory. A `..` segment is refused here too, because no
 * honest caller sends one and an app would only be handed an error later.
 */
export function parseAppOpen(input: unknown, apps: Pick<AppTable, "isKnown">): Parsed {
  if (!input || typeof input !== "object" || Array.isArray(input)) {
    return { ok: false, reason: "Opening an app needs an app name." };
  }
  const { app, path } = input as { app?: unknown; path?: unknown };
  if (typeof app !== "string" || app === "") {
    return { ok: false, reason: "Opening an app needs an app name." };
  }
  if (!apps.isKnown(app)) {
    return { ok: false, reason: `There is no app called ${app} on this node.` };
  }
  if (path === undefined || path === null) return { ok: true, request: { app } };
  if (typeof path !== "string") {
    return { ok: false, reason: "A file to open is given as a path." };
  }
  // Never trimmed: the core does not trim names, so `/notes.md ` and
  // `/notes.md` are two files, and trimming here would open (and save into)
  // the other one.
  if (path.trim() === "" || path.includes("\0") || path.length > MAX_PATH) {
    return { ok: false, reason: "That is not a file path." };
  }
  const body = path.replace(/^[^/:]+:(?=\/)/, "");
  if (body.split("/").includes("..")) {
    return { ok: false, reason: "A path cannot climb out of the working directory." };
  }
  const address = /^[^/]*:\//.test(path) || path.startsWith("/") ? path : `/${path}`;
  return { ok: true, request: { app, path: address } };
}

/**
 * The same request, from an app's frame, which is held to more: the target
 * must be an app installed on this node, never one of the core's own panels
 * (the terminal, the Assistant). A frame opening a terminal would be an app
 * spawning a shell on the host by asking the desktop for one.
 * (docs/APP-UI-ISOLATION.md §4.5.)
 */
export function parseFrameAppOpen(input: unknown, apps: Pick<AppTable, "isKnown" | "isInstalled">): Parsed {
  const parsed = parseAppOpen(input, apps);
  if (!parsed.ok) return parsed;
  if (!apps.isInstalled(parsed.request.app)) {
    return { ok: false, reason: `There is no app called ${parsed.request.app} installed on this node.` };
  }
  return parsed;
}

/**
 * The URL an app's frame loads.
 *
 * `window` scopes whatever the app keeps on this device (the editor's unsaved
 * text) to the window rather than to the app, so two editor windows do not
 * write over each other. `path` is the file it was opened on, if any.
 */
export function frameSrc(app: string, windowId: string, path?: string | null): string {
  const query = new URLSearchParams({ window: windowId });
  if (path) query.set("path", path);
  return `/apps/${encodeURIComponent(app)}/?${query.toString()}`;
}
