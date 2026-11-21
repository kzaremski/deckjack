import type { AppTable } from "../stores/apps.ts";
import type { CommandsStore } from "../stores/commands.ts";
import { APP_CAN_OPEN, APP_OPEN, FRAME_REPLY, parseFrameAppOpen } from "./open-app.ts";

/**
 * The shell's half of `/kit/shell.js`: an app's frame asking the desktop for
 * something that is not on the bus.
 *
 * Three checks, and the first two turn a message away silently -- the
 * sender is not one of ours, so there is nobody to tell:
 *
 * 1. **Same origin.** The kit posts with `location.origin`; anything else is
 *    not one of ours.
 * 2. **From a frame the shell is hosting.** `AppFramePanel` attaches each
 *    app window's `contentWindow`, and only those may ask. A message from any
 *    other window -- a popup, a tab the shell opened -- is ignored.
 * 3. **A command frames may ask for.** Quick Look, and opening an *installed*
 *    app, optionally on a file (`{ type: "shell.app.open", app, path }`, see
 *    `parseFrameAppOpen` in `open-app.ts`). Never a core panel such as the
 *    terminal or the Assistant, and never any other shell command: those act
 *    on the desktop itself (closing windows, locking down), and an app's UI
 *    has no business driving them.
 *
 * An app-open request that carries an `id` gets an answer posted back to the
 * frame (`deckjack.shell.reply`, with the reason when refused), so the app
 * can fall back or say why rather than wait on a window that never comes.
 * `shell.app.can-open` asks the same question without opening anything, so
 * an app can leave out an "Open with" it cannot honor.
 *
 * Same caveat as the kit: an app's UI runs on the shell's origin, so this is
 * a contract and not a sandbox. It keeps the interface small and named.
 */

export const FRAME_MESSAGE = "deckjack.shell";

/** Shell commands an app's frame may dispatch. */
export const FRAME_COMMANDS: ReadonlySet<string> = new Set(["shell.quicklook.open"]);

export interface FrameMessage {
  origin: string;
  source: unknown;
  data: unknown;
}

export interface FrameBridge {
  /** Accept requests from this frame's window until the returned function is
   * called. */
  attach(frame: unknown): () => void;
  /** Handle one `message` event. True when it dispatched or answered
   * something. */
  handle(event: FrameMessage): boolean;
}

/**
 * `apps` is required for the same reason `createWindows` requires it: the
 * table is per shell, and a bridge built without it could not refuse a name
 * nothing installed.
 */
export function createFrameBridge(deps: {
  commands: CommandsStore;
  origin: string;
  apps: Pick<AppTable, "isKnown" | "isInstalled">;
}): FrameBridge {
  const frames = new Set<unknown>();

  /** Answer a request that asked for one. A frame that cannot be posted to
   * (it navigated away, or a test's stand-in) simply gets no answer. */
  function reply(source: unknown, id: unknown, ok: boolean, reason?: string) {
    if (typeof id !== "number" && typeof id !== "string") return;
    const target = source as { postMessage?: (data: unknown, origin: string) => void };
    if (typeof target.postMessage !== "function") return;
    const answer: Record<string, unknown> = { type: FRAME_REPLY, id, ok };
    if (reason) answer.reason = reason;
    try {
      target.postMessage(answer, deps.origin);
    } catch {
      // The frame went away between asking and being answered.
    }
  }

  return {
    attach(frame) {
      if (frame) frames.add(frame);
      return () => void frames.delete(frame);
    },
    handle(event) {
      if (event.origin !== deps.origin) return false;
      if (!event.source || !frames.has(event.source)) return false;
      const data = event.data as
        | { type?: unknown; command?: unknown; params?: unknown; id?: unknown; app?: unknown }
        | null;
      if (!data || typeof data !== "object") return false;
      if (data.type === APP_CAN_OPEN) {
        const ok = typeof data.app === "string" && deps.apps.isInstalled(data.app);
        reply(event.source, data.id, ok);
        return true;
      }
      if (data.type === APP_OPEN) {
        const parsed = parseFrameAppOpen(data, deps.apps);
        if (!parsed.ok) {
          reply(event.source, data.id, false, parsed.reason);
          return false;
        }
        void deps.commands.dispatch(APP_OPEN, { ...parsed.request });
        reply(event.source, data.id, true);
        return true;
      }
      if (data.type !== FRAME_MESSAGE) return false;
      if (typeof data.command !== "string" || !FRAME_COMMANDS.has(data.command)) return false;
      const params =
        data.params && typeof data.params === "object" && !Array.isArray(data.params)
          ? (data.params as Record<string, unknown>)
          : {};
      void deps.commands.dispatch(data.command, params);
      return true;
    },
  };
}
