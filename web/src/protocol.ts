/**
 * The bus wire format.
 *
 * This mirrors `crates/deckjack-core/src/ws.rs` and nothing else. The Rust
 * types are canonical — they are what the server actually serializes — and
 * the design has this file generated from them eventually (`ts-rs` or
 * `specta`). Until that generator exists it is maintained by hand, so a
 * change on either side has to be made on both.
 *
 * Serde writes internally-tagged enums, so every frame is a flat object with
 * a discriminator:
 *
 *   ClientFrame / ServerFrame   tag `type`,   snake_case values
 *   Outcome                     tag `status`, snake_case values
 */

/** Bumped when a frame changes shape. The server refuses a mismatch. */
export const PROTOCOL_VERSION = 1;

/** Opaque. The server echoes it on every frame answering a call. */
export type CorrelationId = string;

// --- client -> server -------------------------------------------------------

/** Must be the first frame; anything else closes the socket. */
export interface Hello {
  type: "hello";
  protocol: number;
  client?: string;
  /**
   * What the operator calls this device ("Chrome on Pixel"). Shown to the
   * other devices while this connection holds the seat; the node sanitizes
   * it and decides nothing by it.
   */
  device?: string;
}

/**
 * Run a command. There is no `confirmed` field: the node refuses an invoke
 * that carries one. A confirmation is a ticket the node issues and a
 * `confirm` frame redeems.
 */
export interface Invoke {
  type: "invoke";
  id: CorrelationId;
  command: string;
  params?: unknown;
  /**
   * The call is made for this installed app's window, and the node holds it
   * to that app's scope (docs/APP-UI-ISOLATION.md §4.6). Sent by the UI
   * protocol's renderer for a widget's bound command.
   */
  as_app?: string;
}

/**
 * Run a call the operator just confirmed. `ticket` is what the node's
 * `needs_confirmation` answer carried; `command` and `params` are the same
 * call again, which the node checks against what it asked about. Good once,
 * for a minute, on the socket that was asked. `id` is a fresh correlation id.
 */
export interface Confirm {
  type: "confirm";
  id: CorrelationId;
  ticket: string;
  command: string;
  params?: unknown;
  /** The same `as_app` the invoke carried: the ticket is bound to it. */
  as_app?: string;
}

/**
 * Cooperative. The command sees the signal and unwinds, so a `response`
 * still arrives for this id — do not clean up on sending this.
 */
export interface Cancel {
  type: "cancel";
  id: CorrelationId;
}

/**
 * Liveness probe. The node answers with `pong`, echoing `token`.
 *
 * WebSocket already has ping/pong at the protocol level, and it is no use
 * here: the browser answers those itself in the network stack and never tells
 * the page. So a node that vanished without a FIN leaves the socket `OPEN` to
 * JavaScript until TCP gives up — minutes — and the shell spends every one of
 * them claiming it is connected. This frame is the only way a page can learn
 * that a whole round trip still completes.
 */
export interface Ping {
  type: "ping";
  token?: string;
}

/**
 * The UI protocol's per-window channel: open a window an app describes, send
 * it an event, close it. See `src/uiproto/`.
 */
export interface UiUp {
  type: "ui";
  window: string;
  body: unknown;
}

export type ClientFrame = Hello | Invoke | Confirm | Cancel | Ping | UiUp;

// --- server -> client -------------------------------------------------------

export interface Welcome {
  type: "welcome";
  protocol: number;
  connection: number;
  node_id: string;
  host_name: string;
}

/** What a command reports while it runs. Every field is optional. */
export interface Progress {
  fraction?: number;
  message?: string;
  log?: string[];
}

/**
 * `seq` counts from 0 per correlation id and is the point of the frame:
 * progress is dropped server-side when a client is behind, so a gap in `seq`
 * is how you learn you missed some rather than rendering a stale bar.
 */
export interface ProgressFrame {
  type: "progress";
  id: CorrelationId;
  seq: number;
  progress: Progress;
}

export interface CommandError {
  code: string;
  message: string;
  detail?: unknown;
}

export type Outcome =
  | { status: "ok"; value: unknown }
  | { status: "error"; error: CommandError };

/** Exactly one per accepted invoke. */
export interface ResponseFrame {
  type: "response";
  id: CorrelationId;
  outcome: Outcome;
  elapsed_ms: number;
}

/** Unsolicited: a window moved, an export finished, an approval is waiting. */
export interface EventFrame {
  type: "event";
  topic: string;
  payload: unknown;
  at_ms: number;
}

/** The answer to `ping`, echoing its token. Carries no state by design. */
export interface PongFrame {
  type: "pong";
  token?: string;
}

/** The UI protocol's per-window channel, node to this connection only. */
export interface UiDown {
  type: "ui";
  window: string;
  body: unknown;
}

export type ServerFrame = Welcome | ProgressFrame | ResponseFrame | EventFrame | PongFrame | UiDown;

export function isUi(f: ServerFrame): f is UiDown {
  return f.type === "ui";
}

export function isWelcome(f: ServerFrame): f is Welcome {
  return f.type === "welcome";
}
export function isPong(f: ServerFrame): f is PongFrame {
  return f.type === "pong";
}
export function isProgress(f: ServerFrame): f is ProgressFrame {
  return f.type === "progress";
}
export function isResponse(f: ServerFrame): f is ResponseFrame {
  return f.type === "response";
}
export function isEvent(f: ServerFrame): f is EventFrame {
  return f.type === "event";
}

/**
 * Risk is declared by the backend per command and never inferred here. An
 * adapter bug must not be able to downgrade a destructive command, so the
 * browser treats whatever it is told as authoritative and shows it.
 */
export type Risk = "read_only" | "write" | "cross_project" | "outbound" | "destructive";

/**
 * A command as the palette shows it.
 *
 * `summary` and `group` are presentation only: the server does not send them,
 * the shell adds them for its own built-in commands, and a bus-provided
 * command simply has neither. They live here rather than on `CommandEntry`
 * so that a future generated version of this file can carry them if the
 * backend ever starts declaring them.
 */
export interface CommandSpec {
  id: string;
  title: string;
  risk: Risk;
  node?: string;
  enabled?: boolean;
  summary?: string;
  group?: string;
  /** Extra terms the palette matches on, beyond the title. */
  keywords?: string[];
}

/**
 * A window as the server holds it.
 *
 * Windows are server state so every device sees the same ones, and the two
 * layouts are kept separately so using the phone never scrambles the desktop
 * arrangement.
 */
export interface WireWindow {
  id: string;
  app: string;
  title: string;
  minWidth: number;
  minHeight: number;
  desktop: {
    x: number;
    y: number;
    w: number;
    h: number;
    z: number;
    minimized: boolean;
    maximized: boolean;
  };
  /**
   * Present on a streamed native app's window (`crates/deckjack-core/src/
   * stream.rs`): which app, the picture's size and, for a popup, its parent
   * and offset. Parsed by `parseStreamMeta` in `shell/stream.ts`.
   */
  stream?: unknown;
}

// --- client-side normalizations --------------------------------------------
//
// Not wire types. The frames above are what the socket carries; these are the
// shapes the shell's components work in, so a change to the wire format is
// absorbed in `stores/bus.ts` rather than rippling through every caller.

/** A command's result, flattened for callers. */
export type CommandOutcome =
  | { id: CorrelationId; status: "ok"; value?: unknown }
  /** `code` is the node's stable error code, when it sent one. */
  | { id: CorrelationId; status: "error"; message: string; code?: string }
  /** Above the risk threshold for its issuer: waiting in the Inbox. */
  | { id: CorrelationId; status: "queued"; inboxId: string }
  /** Outside the session's grant. Refused, not queued. */
  | { id: CorrelationId; status: "refused"; message: string }
  /**
   * The operator has not been asked yet. `prompt` is the bus's own wording,
   * not the client's: the backend declared the risk and wrote the sentence,
   * so a client cannot describe a destructive command as something milder.
   * `target` is what the call acts on, rendered by the node from the
   * validated params, as plain text. `ticket` redeems the confirmation.
   */
  | {
      id: CorrelationId;
      status: "confirm";
      prompt: string;
      risk: string;
      target?: string;
      ticket: string;
    };

/**
 * What `bus.call` resolves to.
 *
 * `confirm` is absent: the bus asks the operator and re-invokes itself, so a
 * caller never sees a half-finished dispatch it would have to drive forward.
 */
export type CallResult = Exclude<CommandOutcome, { status: "confirm" }>;

/**
 * The single seat, as the node describes it to one connection: the payload
 * of the `seat` event and the answer to every `seat.*` command. Mirrors
 * `seat::Status::to_json` in `crates/deckjack-core/src/seat.rs`.
 */
export interface SeatStatus {
  /** This connection holds the seat. */
  held: boolean;
  /** This connection would get it without taking it from anyone. */
  claimable: boolean;
  /** Who has it, or null when it is vacant. */
  holder: {
    device: string;
    /** When they took it, milliseconds since the epoch. */
    since_ms: number;
    /** False while it is held for a device whose connection dropped. */
    connected: boolean;
  } | null;
}

/** Server pushes, normalized by topic into something the stores switch on. */
export type ServerEvent =
  | { kind: "windows"; windows: WireWindow[] }
  | { kind: "commands"; commands: CommandSpec[] }
  | { kind: "inbox"; pending: number }
  | { kind: "node"; name: string; generation: number }
  | { kind: "lockdown"; active: boolean }
  | { kind: "seat"; seat: SeatStatus }
  /** A UI protocol frame for one window. */
  | { kind: "ui"; window: string; body: unknown }
  /** Anything the shell does not yet model, kept so nothing is silently lost. */
  | { kind: "other"; topic: string; payload: unknown };
