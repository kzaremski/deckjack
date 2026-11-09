import { computed, ref, shallowRef, type ComputedRef, type Ref } from "vue";

import {
  PROTOCOL_VERSION,
  isEvent,
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
 * The server side does not exist yet (there is sign-in and nothing behind
 * it), so the bus starts `detached`: calls resolve locally as `ok` and the
 * stores keep their optimistic state. That is a scaffold, not a mock — when
 * `/ws` answers, nothing in the components changes. The one thing it must
 * never do is pretend a *destructive* command succeeded, so those are
 * refused while detached.
 */

export type BusStatus = "connecting" | "open" | "detached" | "closed";

/** Shows the bus's own prompt and resolves true when the operator accepts. */
export type ConfirmAsker = (input: {
  command: string;
  prompt: string;
  risk: string;
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
  };
}

export interface Bus {
  status: Ref<BusStatus>;
  /** True while the shell is running without a server behind it. */
  detached: ComputedRef<boolean>;
  node: Ref<{ name: string; generation: number } | null>;
  lastError: Ref<string | null>;
  call(
    id: string,
    params?: Record<string, unknown>,
    node?: string,
    onProgress?: (progress: Progress, seq: number) => void,
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
      return {
        id,
        status: "confirm",
        prompt: String((v as { prompt?: string }).prompt ?? "Run this command?"),
        risk: String((v as { risk?: string }).risk ?? "destructive"),
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

export function createBus(env: BusEnv): Bus {
  const status = ref<BusStatus>("connecting");
  const node = shallowRef<{ name: string; generation: number } | null>(null);
  const lastError = ref<string | null>(null);
  const handlers = new Set<(event: ServerEvent) => void>();
  const pending = new Map<CorrelationId, (outcome: CommandOutcome) => void>();
  const progressHandlers = new Map<CorrelationId, (p: Progress, seq: number) => void>();

  let socket: SocketLike | null = null;
  let askToConfirm: ConfirmAsker | null = null;
  let seq = 0;
  let attempt = 0;
  let cancelRetry: (() => void) | null = null;

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
      send({ type: "hello", protocol: PROTOCOL_VERSION, client: "deckjack-shell" });
    };

    sock.onmessage = (ev) => {
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
      // Every in-flight call has to be answered or callers hang forever.
      for (const [key, resolve] of pending) {
        resolve({ id: key, status: "error", message: "connection to the shell node closed" });
      }
      pending.clear();
      progressHandlers.clear();
      status.value = status.value === "open" ? "connecting" : "detached";
      scheduleReconnect();
    };
  }

  return {
    status,
    detached: computed(() => status.value !== "open"),
    node,
    lastError,
    async call(id, params, _target, onProgress) {
      seq += 1;
      const correlation = `c${seq}`;

      if (status.value !== "open" || !socket) {
        // Optimistic UI for local shell state is the point: window moves,
        // theme changes and layout changes are the operator's own and are
        // allowed to proceed. Anything that would touch a real machine is
        // not, and says why instead of lying.
        if (id.startsWith("shell.")) return { id: correlation, status: "ok" };
        return {
          id: correlation,
          status: "error",
          message: `no command bus: ${id} needs the shell node, which is not connected yet`,
        };
      }

      const invoke = (corr: CorrelationId, confirmed: boolean) =>
        new Promise<CommandOutcome>((resolve) => {
          pending.set(corr, resolve);
          if (onProgress) progressHandlers.set(corr, onProgress);
          send({
            type: "invoke",
            id: corr,
            command: id,
            params: params ?? {},
            confirmed,
          });
        });

      const first: CommandOutcome = await invoke(correlation, false);

      // The bus asked rather than refused. Show ITS prompt — the backend
      // declared the risk and wrote the wording, and a client that invented
      // its own could describe a destructive command as something milder.
      if (first.status === "confirm") {
        if (!askToConfirm) {
          return { id: correlation, status: "refused", message: first.prompt };
        }
        const ok = await askToConfirm({
          command: id,
          prompt: first.prompt,
          risk: first.risk,
        });
        if (!ok) {
          return { id: correlation, status: "refused", message: "not confirmed" };
        }
        seq += 1;
        const second = await invoke(`c${seq}`, true);
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
    close() {
      cancelRetry?.();
      status.value = "closed";
      socket?.close();
      socket = null;
    },
  };
}
