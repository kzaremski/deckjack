/**
 * The app UI kit: one way for an app's web UI to reach the command bus.
 *
 * An app's UI is served by the core at `/apps/<name>/`, on the core's own
 * origin. That is the whole reason it can talk to the bus at all: the socket
 * at `/ws` is same-origin, so the session cookie goes with it and the app's
 * UI needs no address, no token and no credentials of its own. It never
 * connects to its own backend — the core is always in the middle, which is
 * what keeps sign-in, lockdown and attribution in one place.
 *
 * This file is served from the core's shell bundle rather than embedded in
 * each app, so there is one copy of the wire format rather than one per app.
 * It mirrors `crates/deckjack-core/src/ws.rs`; see `web/src/protocol.ts` for
 * the typed version the shell itself uses.
 */

const PROTOCOL = 1;

/** Frames are correlated by an id the client mints. */
let nextId = 1;

export class Bus {
  constructor() {
    this.socket = null;
    this.pending = new Map();
    this.events = new Map();
    this.ready = null;
    this.node = null;
  }

  /** Connect and complete the handshake. Resolves with the welcome frame. */
  connect() {
    if (this.ready) return this.ready;
    this.ready = new Promise((resolve, reject) => {
      const scheme = location.protocol === "https:" ? "wss:" : "ws:";
      const socket = new WebSocket(`${scheme}//${location.host}/ws`);
      this.socket = socket;

      socket.addEventListener("open", () => {
        socket.send(JSON.stringify({ type: "hello", protocol: PROTOCOL }));
      });

      socket.addEventListener("message", (event) => {
        let frame;
        try {
          frame = JSON.parse(event.data);
        } catch {
          return;
        }
        if (frame.type === "welcome") {
          this.node = frame;
          resolve(frame);
          return;
        }
        if (frame.type === "response") {
          const waiter = this.pending.get(frame.id);
          if (!waiter) return;
          this.pending.delete(frame.id);
          const outcome = frame.outcome ?? {};
          if (outcome.status === "ok") waiter.resolve(outcome.value);
          else waiter.reject(new Error(outcome.error?.message ?? "the command failed"));
          return;
        }
        if (frame.type === "progress") {
          const waiter = this.pending.get(frame.id);
          waiter?.progress?.(frame.progress ?? {});
          return;
        }
        if (frame.type === "event") {
          for (const fn of this.events.get(frame.topic) ?? []) fn(frame.payload);
        }
      });

      const fail = () => {
        // Every in-flight call is rejected rather than left hanging: a panel
        // that waits forever on a closed socket looks like a frozen app.
        for (const waiter of this.pending.values()) {
          waiter.reject(new Error("the connection to Deckjack closed"));
        }
        this.pending.clear();
        this.ready = null;
        this.socket = null;
        reject(new Error("the connection to Deckjack closed"));
      };
      socket.addEventListener("close", fail);
      socket.addEventListener("error", fail);
    });
    return this.ready;
  }

  /**
   * Invoke a command. Resolves with its value, rejects with its error.
   *
   * A command the operator must confirm comes back as an outcome the shell
   * handles, not this: an app's UI inside a window never draws its own
   * confirmation dialog, because then an app would be writing the sentence
   * that describes its own destructive command.
   */
  async invoke(command, params = {}, onProgress) {
    await this.connect();
    const id = String(nextId++);
    return new Promise((resolve, reject) => {
      this.pending.set(id, { resolve, reject, progress: onProgress });
      this.socket.send(JSON.stringify({ type: "invoke", id, command, params }));
    });
  }

  /** Listen for a server push by topic. */
  on(topic, fn) {
    const list = this.events.get(topic) ?? [];
    list.push(fn);
    this.events.set(topic, list);
    return () => this.events.set(topic, (this.events.get(topic) ?? []).filter((f) => f !== fn));
  }
}

/** One bus per document. An app's UI has no reason to open two sockets. */
export const bus = new Bus();

/** Escape text for innerHTML. Apps render file names and note titles. */
export function text(value) {
  return String(value ?? "").replace(
    /[&<>"']/g,
    (c) => ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;", "'": "&#39;" })[c],
  );
}

/**
 * Show a failure where the person is looking.
 *
 * Apps are small and their failures are mostly one of two things: the command
 * was refused, or the socket went. Either way the honest thing is a line of
 * text in the panel rather than a console message nobody reads.
 */
export function failure(host, error) {
  const line = document.createElement("p");
  line.className = "dj-error";
  line.setAttribute("role", "alert");
  line.textContent = error instanceof Error ? error.message : String(error);
  host.replaceChildren(line);
}
