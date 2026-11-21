import { computed, ref, shallowRef, type ComputedRef, type Ref } from "vue";

import {
  PROTOCOL_VERSION,
  isEvent,
  isPong,
  isProgress,
  isResponse,
  isWelcome,
  type ClientFrame,
  type CallResult,
  type CommandOutcome,
  type CommandSpec,
  type CorrelationId,
  type Progress,
  type ServerEvent,
  type ServerFrame,
  type WireWindow,
} from "../protocol.ts";

/**
 * The client half of the one command bus.
 *
 * Everything the shell does goes through `call`: a click, a shortcut, a
 * palette entry and an agent's tool call all end up as the same typed
 * command, so there is no path through the UI that does something the bus
 * cannot describe.
 *
 * The bus is the only thing that decides whether the shell is working. When
 * it is not `open`, *nothing* is: every call is refused and says so.
 *
 * It used to resolve `shell.*` calls locally as `ok` while disconnected, on
 * the grounds that the server side did not exist yet. It does now, and that
 * scaffold had become a lie with a specific shape: every window the operator
 * opened, moved or closed reported success to a node that never heard about
 * it, so the shell looked alive and lost the work. A disconnected workspace
 * that refuses is honest; one that accepts is a workspace you cannot trust
 * when it reconnects.
 *
 * Liveness is measured, not assumed — see `HEARTBEAT_MS`.
 */

export type BusStatus = "connecting" | "open" | "detached" | "closed";

/**
 * Shows the bus's own prompt and resolves true when the operator accepts.
 *
 * `target` is what the call acts on, as the node rendered it. Plain text:
 * show it as text, never as markup. The ticket is deliberately not passed:
 * whoever draws the dialog has no use for it, and the bus is the only thing
 * that redeems one.
 */
export type ConfirmAsker = (input: {
  command: string;
  prompt: string;
  risk: string;
  target?: string;
}) => Promise<boolean>;

/** A socket, injected so the bus can be driven without a server. */
export interface SocketLike {
  send(data: string): void;
  close(): void;
  onopen: ((this: unknown, ev: unknown) => void) | null;
  onclose: ((this: unknown, ev: unknown) => void) | null;
  onerror: ((this: unknown, ev: unknown) => void) | null;
  onmessage: ((this: unknown, ev: { data: unknown }) => void) | null;
}

export interface BusEnv {
  /** Opens a socket to the shell node, or throws. */
  connect(url: string): SocketLike;
  url: string;
  now(): number;
  /** Schedules a reconnect; injected so backoff is testable without waiting. */
  delay(ms: number, fn: () => void): () => void;
  /**
   * Whether the device has a network at all.
   *
   * Separate from the bus's own status because the two failures want
   * different words: "this device is offline" is the operator's wifi, while
   * "the node is unreachable" with a network present means the machine their
   * work lives on is down. Telling them to check the wrong one wastes their
   * time.
   */
  online(): boolean;
  /** Subscribes to network up/down; returns an unsubscribe. */
  watchOnline(fn: (online: boolean) => void): () => void;
}

export function browserBusEnv(path = "/ws"): BusEnv {
  const scheme = location.protocol === "https:" ? "wss:" : "ws:";
  return {
    connect: (url) => new WebSocket(url) as unknown as SocketLike,
    url: `${scheme}//${location.host}${path}`,
    now: () => Date.now(),
    delay(ms, fn) {
      const handle = window.setTimeout(fn, ms);
      return () => window.clearTimeout(handle);
    },
    // `navigator.onLine` is only trustworthy when it says false: a captive
    // portal reports true. So it is used to explain a failure the bus already
    // detected, never to decide that one happened.
    online: () => navigator.onLine,
    watchOnline(fn) {
      const up = () => fn(true);
      const down = () => fn(false);
      window.addEventListener("online", up);
      window.addEventListener("offline", down);
      return () => {
        window.removeEventListener("online", up);
        window.removeEventListener("offline", down);
      };
    },
  };
}

export interface Bus {
  status: Ref<BusStatus>;
  /** True whenever the shell is not connected to its node. */
  detached: ComputedRef<boolean>;
  node: Ref<{ name: string; generation: number } | null>;
  lastError: Ref<string | null>;
  /** When a frame last arrived, by `env.now()`. Null before the first one. */
  lastSeenAt: Ref<number | null>;
  /** Last measured round trip in ms, or null if no pong has come back yet. */
  rtt: Ref<number | null>;
  /** The device's network, as distinct from whether the node answers. */
  online: Ref<boolean>;
  /** When the next automatic attempt is due, by `env.now()`. */
  retryAt: Ref<number | null>;
  /** Try again now, instead of waiting out the backoff. */
  reconnectNow(): void;
  call(
    id: string,
    params?: Record<string, unknown>,
    node?: string,
    onProgress?: (progress: Progress, seq: number) => void,
    /**
     * `signal` aborting asks the node to cancel the call, the same as
     * `cancel` with its correlation id, which a caller does not otherwise
     * learn until the call is over. A response still arrives and resolves
     * the promise. Aborted before the call starts, nothing is sent.
     */
    options?: { signal?: AbortSignal },
  ): Promise<CallResult>;
  /** Ask a running call to stop. A response still arrives for it. */
  cancel(correlation: CorrelationId): void;
  /**
   * How to ask the operator to confirm a destructive command.
   *
   * Injected rather than imported so the bus does not depend on the UI
   * store. Until one is set, a command needing confirmation is refused —
   * which is the safe direction to fail.
   */
  onConfirm(ask: ConfirmAsker): void;
  on(handler: (event: ServerEvent) => void): () => void;
  connect(): void;
  close(): void;
}

/**
 * Flatten a wire outcome into what callers switch on.
 *
 * `queued` and `needs_confirmation` come back as successful dispatches whose
 * value says the command did not run — the bus admitting a call and the
 * command having run are different things, and the distinction matters most
 * for exactly the commands that get queued.
 */
function flatten(id: CorrelationId, outcome: import("../protocol.ts").Outcome): CommandOutcome {
  if (outcome.status === "error") {
    const refused = outcome.error.code === "refused";
    return refused
      ? { id, status: "refused", message: outcome.error.message }
      : { id, status: "error", message: outcome.error.message };
  }
  const v = outcome.value as { status?: string; pending?: string } | null;
  if (v && typeof v === "object") {
    if (v.status === "queued") return { id, status: "queued", inboxId: v.pending ?? "" };
    if (v.status === "needs_confirmation") {
      const asked = v as { prompt?: string; risk?: string; target?: unknown; ticket?: unknown };
      return {
        id,
        status: "confirm",
        prompt: String(asked.prompt ?? "Run this command?"),
        risk: String(asked.risk ?? "destructive"),
        target: typeof asked.target === "string" ? asked.target : undefined,
        // Empty when the node sent none, which `call` treats as nothing to
        // redeem rather than redeeming nothing.
        ticket: typeof asked.ticket === "string" ? asked.ticket : "",
      };
    }
  }
  return { id, status: "ok", value: outcome.value };
}

/** Map a server topic onto the events the stores already understand. */
function topicToEvent(topic: string, payload: unknown): ServerEvent {
  switch (topic) {
    case "windows":
      return { kind: "windows", windows: payload as WireWindow[] };
    case "commands":
      return { kind: "commands", commands: payload as CommandSpec[] };
    case "inbox":
      return { kind: "inbox", pending: (payload as { pending: number }).pending };
    case "lockdown":
      return { kind: "lockdown", active: (payload as { active: boolean }).active };
    default:
      return { kind: "other", topic, payload };
  }
}

/** Reconnect backoff, capped: a laptop that sleeps should not hammer the node. */
const BACKOFF_MS = [250, 500, 1000, 2000, 5000, 10000];

/**
 * How often to prove the connection is alive, and how long silence is allowed
 * to last before it is declared dead.
 *
 * `readyState === OPEN` only means TCP has not given up. A node that was
 * killed, a laptop lid that closed, a phone that changed networks: none send
 * a FIN, so the socket stays "open" for minutes while every command sent into
 * it disappears. The only reliable evidence is a round trip that completed.
 *
 * The timeout is deliberately more than two beats. One missed pong on a phone
 * handing off between cells is normal and must not tear down a working
 * session; three missed in a row is a connection that is not coming back on
 * its own.
 */
const HEARTBEAT_MS = 8000;
const SILENCE_IS_DEAD_MS = 26000;

export function createBus(env: BusEnv): Bus {
  const status = ref<BusStatus>("connecting");
  const node = shallowRef<{ name: string; generation: number } | null>(null);
  const lastError = ref<string | null>(null);
  const handlers = new Set<(event: ServerEvent) => void>();
  const pending = new Map<CorrelationId, (outcome: CommandOutcome) => void>();
  const progressHandlers = new Map<CorrelationId, (p: Progress, seq: number) => void>();

  const lastSeenAt = ref<number | null>(null);
  const rtt = ref<number | null>(null);
  const online = ref(env.online());
  const retryAt = ref<number | null>(null);

  let socket: SocketLike | null = null;
  let askToConfirm: ConfirmAsker | null = null;
  let seq = 0;
  let attempt = 0;
  let cancelRetry: (() => void) | null = null;
  let cancelBeat: (() => void) | null = null;
  let pingSeq = 0;
  let sentAt = new Map<string, number>();

  function stopHeartbeat() {
    cancelBeat?.();
    cancelBeat = null;
    sentAt = new Map();
  }

  /**
   * Answer everything still waiting. A caller that is never answered awaits
   * forever, so the UI shows a command running against a node that is gone.
   */
  function failPending(message: string) {
    for (const [key, resolve] of pending) {
      resolve({ id: key, status: "error", message });
    }
    pending.clear();
    progressHandlers.clear();
  }

  /**
   * Abandon this connection and start trying for a new one.
   *
   * Everything here has to happen whether or not the socket cooperates. A
   * socket the heartbeat condemned is, by definition, one that is not
   * reporting what happens to it — so waiting for its `onclose` to answer
   * in-flight calls would leave them hanging exactly when the node is gone.
   * Its handlers are detached first for the same reason in reverse: if it
   * *does* fire `onclose` afterwards, the close path must not run twice and
   * advance the reconnect backoff a second time, which would turn the first
   * prompt retry into a longer wait nobody earned.
   */
  /**
   * Severs a socket so completely that it cannot speak again.
   *
   * Detaching the handlers before closing is the load-bearing half. A socket
   * whose `onclose` still fires gets to rewrite `status` and schedule a
   * reconnect *after* the caller has already decided what should happen, so
   * every deliberate transition has to take the handlers away first or be
   * silently overruled by the socket it just discarded.
   */
  function sever(sock: SocketLike | null) {
    if (!sock) return;
    sock.onopen = null;
    sock.onclose = null;
    sock.onerror = null;
    sock.onmessage = null;
    try {
      sock.close();
    } catch {
      // Already gone. Nothing here depends on the close succeeding.
    }
  }

  function giveUp(message: string) {
    const dead = socket;
    socket = null;
    stopHeartbeat();
    rtt.value = null;
    lastError.value = message;
    sever(dead);
    failPending("connection to the shell node closed");
    status.value = "connecting";
    scheduleReconnect();
  }

  /**
   * One beat: declare the connection dead if nothing has arrived for too
   * long, otherwise probe and schedule the next.
   *
   * The check runs *before* the probe so a socket that went silent is caught
   * on the next beat rather than one beat later.
   */
  function beat() {
    if (status.value !== "open" || !socket) {
      stopHeartbeat();
      return;
    }
    const seenAt = lastSeenAt.value ?? env.now();
    if (env.now() - seenAt > SILENCE_IS_DEAD_MS) {
      giveUp(
        online.value ? "the shell node stopped answering" : "this device is offline",
      );
      return;
    }
    pingSeq += 1;
    const token = `p${pingSeq}`;
    sentAt.set(token, env.now());
    // Only the last few round trips are of interest, and an unanswered ping
    // must not accumulate for the life of the connection.
    if (sentAt.size > 8) {
      const oldest = sentAt.keys().next().value;
      if (oldest !== undefined) sentAt.delete(oldest);
    }
    send({ type: "ping", token });
    // Never sleep past the deadline. Beating on a fixed interval meant the
    // death check only ran when a probe happened to come due, so silence that
    // began just after a beat went unnoticed for a further HEARTBEAT_MS — a
    // node could be gone for 34s while the shell still claimed it was there.
    // Landing the next beat exactly on the deadline makes the promise the one
    // SILENCE_IS_DEAD_MS actually states.
    const untilDeadline = seenAt + SILENCE_IS_DEAD_MS - env.now();
    cancelBeat = env.delay(Math.max(250, Math.min(HEARTBEAT_MS, untilDeadline)), beat);
  }

  function startHeartbeat() {
    stopHeartbeat();
    lastSeenAt.value = env.now();
    cancelBeat = env.delay(HEARTBEAT_MS, beat);
  }

  function emit(event: ServerEvent) {
    if (event.kind === "node") node.value = { name: event.name, generation: event.generation };
    for (const h of handlers) h(event);
  }

  function send(frame: ClientFrame) {
    socket?.send(JSON.stringify(frame));
  }

  function scheduleReconnect() {
    const wait = BACKOFF_MS[Math.min(attempt, BACKOFF_MS.length - 1)]!;
    attempt += 1;
    cancelRetry?.();
    retryAt.value = env.now() + wait;
    cancelRetry = env.delay(wait, connect);
  }

  function connect() {
    cancelRetry?.();
    cancelRetry = null;
    status.value = "connecting";
    let sock: SocketLike;
    try {
      sock = env.connect(env.url);
    } catch (e) {
      lastError.value = e instanceof Error ? e.message : String(e);
      status.value = "detached";
      scheduleReconnect();
      return;
    }
    socket = sock;

    sock.onopen = () => {
      attempt = 0;
      status.value = "open";
      lastError.value = null;
      retryAt.value = null;
      send({ type: "hello", protocol: PROTOCOL_VERSION, client: "deckjack-shell" });
      startHeartbeat();
    };

    sock.onmessage = (ev) => {
      // Any frame is proof of life, whatever it turns out to be — including
      // one that fails to parse, which still reached us from a live node.
      lastSeenAt.value = env.now();
      if (typeof ev.data !== "string") return;
      let frame: ServerFrame;
      try {
        frame = JSON.parse(ev.data) as ServerFrame;
      } catch {
        // Untrusted content stays data: a frame we cannot parse is dropped,
        // never evaluated and never allowed to abort the connection.
        lastError.value = "unparseable frame from the shell node";
        return;
      }

      if (isWelcome(frame)) {
        // The node identifies itself in the handshake, so the shell can show
        // which machine is hosting without a second round trip.
        emit({ kind: "node", name: frame.host_name, generation: frame.connection });
        return;
      }

      if (isPong(frame)) {
        // Matched by token so a pong that arrives after its beat does not get
        // credited to a later, faster one and flatter the measured latency.
        const token = frame.token;
        if (token !== undefined) {
          const at = sentAt.get(token);
          if (at !== undefined) {
            rtt.value = env.now() - at;
            sentAt.delete(token);
          }
        }
        return;
      }

      if (isProgress(frame)) {
        progressHandlers.get(frame.id)?.(frame.progress, frame.seq);
        return;
      }

      if (isResponse(frame)) {
        const resolve = pending.get(frame.id);
        pending.delete(frame.id);
        progressHandlers.delete(frame.id);
        resolve?.(flatten(frame.id, frame.outcome));
        return;
      }

      if (isEvent(frame)) emit(topicToEvent(frame.topic, frame.payload));
    };

    sock.onerror = () => {
      lastError.value = "the shell node's command bus is unreachable";
    };

    sock.onclose = () => {
      socket = null;
      stopHeartbeat();
      rtt.value = null;
      failPending("connection to the shell node closed");
      status.value = status.value === "open" ? "connecting" : "detached";
      scheduleReconnect();
    };
  }

  // A network that just came back is worth one immediate attempt: the backoff
  // may be sitting on a ten second wait for a node that is reachable again.
  env.watchOnline((up) => {
    online.value = up;
    if (up && status.value !== "open" && status.value !== "closed") {
      attempt = 0;
      connect();
    }
  });

  return {
    status,
    detached: computed(() => status.value !== "open"),
    node,
    lastError,
    lastSeenAt,
    rtt,
    online,
    retryAt,
    reconnectNow() {
      if (status.value === "open") return;
      attempt = 0;
      cancelRetry?.();
      cancelRetry = null;
      retryAt.value = null;
      connect();
    },
    async call(id, params, _target, onProgress, options) {
      seq += 1;
      const correlation = `c${seq}`;
      const signal = options?.signal;

      if (signal?.aborted) {
        return { id: correlation, status: "error", message: `${id} was cancelled before it ran` };
      }

      if (status.value !== "open" || !socket) {
        // No exception for `shell.*`. Those are exactly the window opens,
        // moves and closes whose record lives on the node, so resolving them
        // `ok` here is the shell telling the operator it did something it did
        // not do. Commands that are genuinely client-only — the palette, the
        // theme, the density, the layout mode — never reach the bus at all,
        // so refusing everything here costs them nothing.
        return {
          id: correlation,
          status: "error",
          message: online.value
            ? `not connected to ${env.url.replace(/^wss?:\/\//, "").replace(/\/ws$/, "")}: ${id} was not run`
            : `this device is offline: ${id} was not run`,
        };
      }

      /**
       * Send one frame for this call and await its answer. The first is an
       * invoke; a confirmation is the same call again, as a `confirm` frame
       * carrying the node's ticket. There is no other way to say "the
       * operator agreed": the node refuses an invoke that claims it.
       */
      const exchange = (corr: CorrelationId, ticket: string | null) =>
        new Promise<CommandOutcome>((resolve) => {
          // Only while this call is still waiting: the listener goes when
          // the call is answered, so a cancel never names an id the node
          // has finished with.
          const onAbort = () => {
            if (status.value === "open") send({ type: "cancel", id: corr });
          };
          signal?.addEventListener("abort", onAbort, { once: true });
          pending.set(corr, (outcome) => {
            signal?.removeEventListener("abort", onAbort);
            resolve(outcome);
          });
          if (onProgress) progressHandlers.set(corr, onProgress);
          send(
            ticket === null
              ? { type: "invoke", id: corr, command: id, params: params ?? {} }
              : { type: "confirm", id: corr, ticket, command: id, params: params ?? {} },
          );
        });

      const first: CommandOutcome = await exchange(correlation, null);

      // The bus asked rather than refused. Show ITS prompt — the backend
      // declared the risk and wrote the wording, and a client that invented
      // its own could describe a destructive command as something milder.
      if (first.status === "confirm") {
        if (!askToConfirm) {
          return { id: correlation, status: "refused", message: first.prompt };
        }
        if (!first.ticket) {
          return {
            id: correlation,
            status: "error",
            message: `the node asked to confirm ${id} but sent nothing to confirm it with; nothing was run`,
          };
        }
        const askedOn = socket;
        const ok = await askToConfirm({
          command: id,
          prompt: first.prompt,
          risk: first.risk,
          target: first.target,
        });
        if (!ok) {
          return { id: correlation, status: "refused", message: "not confirmed" };
        }
        // The dialog may have sat open past the connection that was asked,
        // even one the bus has since replaced with a fresh socket. A ticket
        // is good only on the socket it was issued to, so redeeming it on
        // another would be refused anyway; say what happened instead.
        if (status.value !== "open" || !socket || socket !== askedOn) {
          return {
            id: correlation,
            status: "error",
            message: `the connection closed while ${id} was being confirmed; nothing was run. Run it again`,
          };
        }
        seq += 1;
        const second = await exchange(`c${seq}`, first.ticket);
        // A confirmed invoke that still asks would be a server bug, and
        // looping on it would ask the operator forever. Fail instead.
        return second.status === "confirm"
          ? { id: second.id, status: "refused", message: second.prompt }
          : second;
      }

      return first;
    },
    onConfirm(ask) {
      askToConfirm = ask;
    },
    cancel(correlation) {
      // Cooperative: a response still arrives, so nothing is cleaned up here.
      if (status.value === "open") send({ type: "cancel", id: correlation });
    },
    on(handler) {
      handlers.add(handler);
      return () => handlers.delete(handler);
    },
    connect,
    /**
     * Shuts the bus down for good: nothing retries after this.
     *
     * The socket is severed rather than merely closed. `close()` used to call
     * `socket.close()` with the handlers still attached, so the socket's own
     * `onclose` ran straight afterwards, rewrote the status from "closed" to
     * "detached" and scheduled a reconnect -- a connection the shell had
     * deliberately shut down came back up, `shell.dispose()` left a bus
     * reconnecting forever, and "closed", the one status that means nothing is
     * retrying, was unreachable whenever a socket had been open.
     *
     * In-flight calls are answered for the same reason they are in `giveUp`:
     * their sockets are gone, so nothing else will ever resolve them.
     */
    close() {
      const dead = socket;
      socket = null;
      cancelRetry?.();
      stopHeartbeat();
      retryAt.value = null;
      rtt.value = null;
      sever(dead);
      failPending("the connection to the shell node was closed");
      status.value = "closed";
    },
  };
}
