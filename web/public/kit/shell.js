/**
 * Asking the shell for something only the shell can do.
 *
 * An app's UI reaches the core through the bus (`/kit/bus.js`). A few things
 * are not on the bus because they are the shell's own: Quick Look is an
 * overlay drawn by the desktop, not a command any node runs. For those the
 * app's frame posts a message to the window that hosts it, and the shell
 * dispatches the named command if it is one it lets frames ask for.
 *
 * Be clear about what that is: a **contract**, not a security boundary. An
 * app's UI is served on the shell's own origin, so a frame that wanted to
 * could reach into the parent document directly. The allowlist on the shell's
 * side keeps the interface between apps and the desktop small and named; it
 * does not make a hostile app's UI safe.
 */

const MESSAGE = "deckjack.shell";

/** The open-app message type; the shell's `APP_OPEN`. */
const OPEN_APP = "shell.app.open";
/** Whether an app can be opened; the shell's `APP_CAN_OPEN`. */
const CAN_OPEN = "shell.app.can-open";
/** The shell's answer to a request with an `id`; its `FRAME_REPLY`. */
const REPLY = "deckjack.shell.reply";

/** How long to wait for the shell's answer before taking it as a no. */
const ANSWER_MS = 5000;

let nextId = 0;
/** Requests waiting for the shell's answer, by id. */
const waiting = new Map();

window.addEventListener("message", (event) => {
  // Only the shell hosting this frame, on this origin, can answer.
  if (window.parent === window || event.source !== window.parent) return;
  if (event.origin !== location.origin) return;
  const data = event.data;
  if (!data || data.type !== REPLY || !waiting.has(data.id)) return;
  const settle = waiting.get(data.id);
  waiting.delete(data.id);
  settle(data.ok === true ? { ok: true } : { ok: false, reason: String(data.reason ?? "") });
});

/** Post `message` with an id and resolve with the shell's answer. */
function ask(message) {
  const id = ++nextId;
  return new Promise((resolve) => {
    const timer = setTimeout(() => {
      if (!waiting.delete(id)) return;
      resolve({ ok: false, reason: "The desktop did not answer." });
    }, ANSWER_MS);
    waiting.set(id, (answer) => {
      clearTimeout(timer);
      resolve(answer);
    });
    window.parent.postMessage({ ...message, id }, location.origin);
  });
}

function send(command, params) {
  // Opened directly in its own tab while being built, an app has no shell to
  // ask. Saying so lets the caller fall back instead of posting into itself.
  if (window.parent === window) return false;
  window.parent.postMessage({ type: MESSAGE, command, params }, location.origin);
  return true;
}

export const shell = {
  /**
   * Preview `path` in Quick Look. `siblings` is the folder to step through
   * with the arrow keys, in display order, and should include `path`.
   * Returns false when there is no shell to ask.
   */
  quicklook(path, siblings) {
    return send("shell.quicklook.open", {
      path,
      siblings: Array.isArray(siblings) ? siblings : undefined,
    });
  },

  /**
   * Open the app called `name` (its name in the dock, e.g. `"scribe"`, the Editor),
   * optionally on a file: `path` is a working-directory address such as
   * `"/src/main.rs"`, passed exactly as given. The shell brings that app's
   * window forward if one is open, else opens one, and hands it the path.
   *
   * Returns false when there is no shell to ask. Otherwise a promise of the
   * shell's answer: `{ ok: true }`, or `{ ok: false, reason }` when it
   * refused -- an app that is not installed on this node, one of the core's
   * own panels (a frame may not open the terminal), or a path that is not a
   * path (see `web/src/shell/open-app.ts`). An app that gets a refusal
   * should fall back or say so; nothing else will.
   */
  openApp(name, { path } = {}) {
    if (window.parent === window) return false;
    const message = { type: OPEN_APP, app: name };
    if (typeof path === "string") message.path = path;
    return ask(message);
  },

  /**
   * Whether `openApp(name)` would be honored: the app is installed on this
   * node. A promise of true or false; false with no shell to ask. For
   * leaving out an "Open with" that would only be refused.
   */
  canOpen(name) {
    if (window.parent === window) return Promise.resolve(false);
    return ask({ type: CAN_OPEN, app: name }).then((answer) => answer.ok);
  },
};

// The same object, for a page that would rather not import a module.
window.deckjackShell = shell;

export { MESSAGE, OPEN_APP, CAN_OPEN, REPLY };
