/**
 * Streamed native apps, the browser's half (docs/lanes/8-stream.md).
 *
 * A native Linux app runs under Deckjack's own compositor on the node, and
 * each of its windows reaches the shell as H.264 over a WebSocket of its
 * own (`/stream?window=<id>`), relayed by the core. This module holds the
 * rules, with no DOM: how a picture is framed, how the bitstream is handed
 * to WebCodecs, how a key and a pointer position are turned into what the
 * compositor expects. `components/panels/StreamPanel.vue` is the DOM.
 *
 * Two facts the rules lean on, both measured (spike S3):
 *
 * - **WebKit decodes AVC, not Annex B.** The compositor writes Annex B
 *   (start codes, SPS and PPS in front of every keyframe). Chromium decodes
 *   that as is; WebKit (on Linux, GStreamer underneath) says the config is
 *   supported and then fails every chunk. Both decode AVC: an `avcC`
 *   description built from the keyframe's SPS and PPS, and NAL units
 *   prefixed by their length. So the panel always converts.
 * - **96 dpi, scale 1.** The app sees one 1920x1080 output and never learns
 *   the client's density. Sizes and positions in messages are the app's
 *   logical pixels; the browser scales the picture to the panel.
 */

/** How the core frames a picture: flags, width, height, timestamp. */
export const HEADER_LEN = 9;

/** One picture off the stream socket. */
export interface Picture {
  keyframe: boolean;
  /** The visible size, in the app's logical pixels. */
  width: number;
  height: number;
  timestamp: number;
  /** H.264, Annex B. */
  data: Uint8Array;
}

export function parsePicture(buffer: ArrayBuffer): Picture | null {
  if (buffer.byteLength < HEADER_LEN + 4) return null;
  const view = new DataView(buffer);
  return {
    keyframe: (view.getUint8(0) & 1) === 1,
    width: view.getUint16(1, true),
    height: view.getUint16(3, true),
    timestamp: view.getUint32(5, true),
    data: new Uint8Array(buffer, HEADER_LEN),
  };
}

/** The NAL units of an Annex B stream, start codes removed. */
export function nalUnits(annexB: Uint8Array): Uint8Array[] {
  const units: Uint8Array[] = [];
  let start = -1;
  let i = 0;
  while (i + 2 < annexB.length) {
    const three = annexB[i] === 0 && annexB[i + 1] === 0 && annexB[i + 2] === 1;
    const four = i + 3 < annexB.length && annexB[i] === 0 && annexB[i + 1] === 0 && annexB[i + 2] === 0 && annexB[i + 3] === 1;
    if (three || four) {
      if (start >= 0) units.push(annexB.subarray(start, i));
      i += three ? 3 : 4;
      start = i;
    } else {
      i += 1;
    }
  }
  if (start >= 0 && start < annexB.length) units.push(annexB.subarray(start));
  return units;
}

const NAL_SPS = 7;
const NAL_PPS = 8;

const hex2 = (n: number) => n.toString(16).padStart(2, "0");

/**
 * What a keyframe says the decoder needs: the codec string (`avc1.PPCCLL`,
 * profile, constraints and level from the SPS) and the `avcC` record. Null
 * for a picture with no SPS and PPS, which is not a keyframe the compositor
 * wrote.
 */
export function avcConfig(annexB: Uint8Array): { codec: string; description: Uint8Array } | null {
  const units = nalUnits(annexB);
  const sps = units.find((u) => (u[0]! & 0x1f) === NAL_SPS);
  const pps = units.find((u) => (u[0]! & 0x1f) === NAL_PPS);
  if (!sps || !pps || sps.length < 4) return null;
  const description = new Uint8Array(11 + sps.length + pps.length);
  // configurationVersion, profile, compatibility, level, 4-byte lengths,
  // one SPS.
  description.set([1, sps[1]!, sps[2]!, sps[3]!, 0xff, 0xe1, sps.length >> 8, sps.length & 0xff]);
  description.set(sps, 8);
  description.set([1, pps.length >> 8, pps.length & 0xff], 8 + sps.length);
  description.set(pps, 11 + sps.length);
  return { codec: `avc1.${hex2(sps[1]!)}${hex2(sps[2]!)}${hex2(sps[3]!)}`, description };
}

/** Annex B to AVC: each NAL unit prefixed by its length, SPS and PPS left
 * out (they are in the description). */
export function toAvc(annexB: Uint8Array): Uint8Array {
  const units = nalUnits(annexB).filter((u) => {
    const type = u[0]! & 0x1f;
    return type !== NAL_SPS && type !== NAL_PPS;
  });
  const out = new Uint8Array(units.reduce((n, u) => n + 4 + u.length, 0));
  const view = new DataView(out.buffer);
  let at = 0;
  for (const u of units) {
    view.setUint32(at, u.length);
    out.set(u, at + 4);
    at += 4 + u.length;
  }
  return out;
}

/**
 * `KeyboardEvent.code` to Linux evdev key codes (`input-event-codes.h`).
 *
 * `code` and not `key`: it names the physical key, so what the key means is
 * decided by the compositor's keymap (US), where the app runs, and a
 * browser on a French layout sends the same codes as one on a US layout.
 * Typing other layouts' characters needs IME support, which is deferred.
 */
export const EVDEV: Readonly<Record<string, number>> = {
  Escape: 1,
  Digit1: 2, Digit2: 3, Digit3: 4, Digit4: 5, Digit5: 6, Digit6: 7, Digit7: 8, Digit8: 9, Digit9: 10, Digit0: 11,
  Minus: 12, Equal: 13, Backspace: 14, Tab: 15,
  KeyQ: 16, KeyW: 17, KeyE: 18, KeyR: 19, KeyT: 20, KeyY: 21, KeyU: 22, KeyI: 23, KeyO: 24, KeyP: 25,
  BracketLeft: 26, BracketRight: 27, Enter: 28, ControlLeft: 29,
  KeyA: 30, KeyS: 31, KeyD: 32, KeyF: 33, KeyG: 34, KeyH: 35, KeyJ: 36, KeyK: 37, KeyL: 38,
  Semicolon: 39, Quote: 40, Backquote: 41, ShiftLeft: 42, Backslash: 43,
  KeyZ: 44, KeyX: 45, KeyC: 46, KeyV: 47, KeyB: 48, KeyN: 49, KeyM: 50,
  Comma: 51, Period: 52, Slash: 53, ShiftRight: 54, NumpadMultiply: 55, AltLeft: 56, Space: 57, CapsLock: 58,
  F1: 59, F2: 60, F3: 61, F4: 62, F5: 63, F6: 64, F7: 65, F8: 66, F9: 67, F10: 68,
  NumLock: 69, ScrollLock: 70,
  Numpad7: 71, Numpad8: 72, Numpad9: 73, NumpadSubtract: 74, Numpad4: 75, Numpad5: 76, Numpad6: 77,
  NumpadAdd: 78, Numpad1: 79, Numpad2: 80, Numpad3: 81, Numpad0: 82, NumpadDecimal: 83,
  IntlBackslash: 86, F11: 87, F12: 88, IntlRo: 89,
  NumpadEnter: 96, ControlRight: 97, NumpadDivide: 98, PrintScreen: 99, AltRight: 100,
  Home: 102, ArrowUp: 103, PageUp: 104, ArrowLeft: 105, ArrowRight: 106, End: 107, ArrowDown: 108,
  PageDown: 109, Insert: 110, Delete: 111, NumpadEqual: 117, Pause: 119, NumpadComma: 121,
  IntlYen: 124, MetaLeft: 125, MetaRight: 126, ContextMenu: 127,
  F13: 183, F14: 184, F15: 185, F16: 186, F17: 187, F18: 188, F19: 189, F20: 190,
  F21: 191, F22: 192, F23: 193, F24: 194,
};

export function evdevFor(code: string): number | null {
  return Object.hasOwn(EVDEV, code) ? EVDEV[code]! : null;
}

export interface Box {
  left: number;
  top: number;
  width: number;
  height: number;
}

/**
 * Where a picture of `picture` logical pixels is drawn in a panel of `box`
 * CSS pixels: scaled to fit, centered, never stretched (`object-fit:
 * contain`). The app is configured to the panel's size, so the scale is 1
 * and there are no bars once it has caught up; until then, and for an app
 * that keeps its own size, the picture is letterboxed rather than warped.
 */
export function fit(box: Box, picture: { width: number; height: number }) {
  if (picture.width <= 0 || picture.height <= 0 || box.width <= 0 || box.height <= 0) {
    return { x: box.left, y: box.top, scale: 1 };
  }
  const scale = Math.min(box.width / picture.width, box.height / picture.height);
  return {
    x: box.left + (box.width - picture.width * scale) / 2,
    y: box.top + (box.height - picture.height * scale) / 2,
    scale,
  };
}

/**
 * A pointer position in the panel (client coordinates) as a position in
 * the app's window, in its logical pixels. Null outside the picture.
 */
export function toWindowPoint(
  clientX: number,
  clientY: number,
  box: Box,
  picture: { width: number; height: number },
): { x: number; y: number } | null {
  const at = fit(box, picture);
  const x = (clientX - at.x) / at.scale;
  const y = (clientY - at.y) / at.scale;
  if (!(x >= 0 && y >= 0 && x < picture.width && y < picture.height)) return null;
  return { x, y };
}

/**
 * The same, but held inside the picture instead of refused outside it: for
 * a button's release, and for moves while a button is held, which the
 * pointer capture delivers wherever the pointer has gone. Dropping those
 * would leave the button down in the app.
 */
export function toWindowPointClamped(
  clientX: number,
  clientY: number,
  box: Box,
  picture: { width: number; height: number },
): { x: number; y: number } | null {
  if (picture.width <= 0 || picture.height <= 0) return null;
  const at = fit(box, picture);
  const clamp = (v: number, max: number) => (Number.isFinite(v) ? Math.min(Math.max(v, 0), max) : 0);
  return {
    x: clamp((clientX - at.x) / at.scale, picture.width - 1),
    y: clamp((clientY - at.y) / at.scale, picture.height - 1),
  };
}

/** `PointerEvent.buttons` bits, in DOM `button` numbers: primary,
 * secondary, middle, back, forward. */
const BUTTON_BITS: ReadonlyArray<readonly [bit: number, button: number]> = [
  [1, 0],
  [2, 2],
  [4, 1],
  [8, 3],
  [16, 4],
];

/**
 * The presses and releases between two `buttons` masks. A second button
 * pressed while one is held, or one of two released, arrives as a
 * `pointermove` with a new mask and no `pointerdown` or `pointerup`
 * (Pointer Events, chorded buttons), so every pointer event is diffed.
 */
export function buttonChanges(previous: number, next: number): { button: number; pressed: boolean }[] {
  const out: { button: number; pressed: boolean }[] = [];
  for (const [bit, button] of BUTTON_BITS) {
    const was = (previous & bit) !== 0;
    const is = (next & bit) !== 0;
    if (was !== is) out.push({ button, pressed: is });
  }
  return out;
}

/** evdev codes of the modifier keys: Ctrl, Shift, Alt and Meta, both sides. */
export const MODIFIER_CODES: ReadonlySet<number> = new Set([29, 42, 54, 56, 97, 100, 125, 126]);

/** evdev codes of the Meta keys. */
export const META_CODES: ReadonlySet<number> = new Set([125, 126]);

/**
 * The keys to release when Meta is: every held key that is not a modifier.
 * macOS sends no `keyup` for a key released while Cmd is down, so Cmd+C
 * would otherwise leave C held, and auto-repeating, in the app.
 */
export function releasedWithMeta(pressed: Iterable<number>): number[] {
  return [...pressed].filter((code) => !MODIFIER_CODES.has(code));
}

/** A wheel event's delta in pixels, whatever unit the browser used. */
export function wheelPixels(event: { deltaX: number; deltaY: number; deltaMode: number }): { dx: number; dy: number } {
  // DOM_DELTA_LINE and DOM_DELTA_PAGE, as the compositor counts a notch:
  // 100 px is one wheel notch, about three lines.
  const unit = event.deltaMode === 1 ? 33 : event.deltaMode === 2 ? 800 : 1;
  return { dx: event.deltaX * unit, dy: event.deltaY * unit };
}

/** What the core says about a streamed window, in the window list. */
export interface StreamMeta {
  /** The native app's name. */
  app: string;
  appTitle: string;
  kind: "toplevel" | "popup";
  /** A popup's parent window. */
  parent: string | null;
  /** The picture's size, in the app's logical pixels. */
  width: number;
  height: number;
  /** A popup's offset from its parent's picture's corner. */
  x: number;
  y: number;
}

export function parseStreamMeta(raw: unknown): StreamMeta | null {
  if (!raw || typeof raw !== "object") return null;
  const r = raw as Record<string, unknown>;
  const num = (v: unknown) => (typeof v === "number" && Number.isFinite(v) ? v : 0);
  if (typeof r.app !== "string") return null;
  return {
    app: r.app,
    appTitle: typeof r.appTitle === "string" ? r.appTitle : r.app,
    kind: r.kind === "popup" ? "popup" : "toplevel",
    parent: typeof r.parent === "string" ? r.parent : null,
    width: num(r.width),
    height: num(r.height),
    x: num(r.x),
    y: num(r.y),
  };
}

/** The app id a streamed window is drawn and docked under. */
export function nativeAppId(name: string): string {
  return `native:${name}`;
}

export function isNativeAppId(id: string): boolean {
  return id.startsWith("native:");
}

/**
 * Where a popup's window goes on the desktop: its parent's picture's
 * corner (below the title bar), plus the popup's offset, all scaled the way
 * the parent's picture is drawn.
 */
export function popupRect(
  parent: { x: number; y: number; w: number; h: number },
  titlebar: number,
  parentPicture: { width: number; height: number },
  popup: StreamMeta,
): { x: number; y: number; w: number; h: number } {
  const content = { left: parent.x, top: parent.y + titlebar, width: parent.w, height: Math.max(0, parent.h - titlebar) };
  const at = fit(content, parentPicture);
  return {
    x: Math.round(at.x + popup.x * at.scale),
    y: Math.round(at.y + popup.y * at.scale),
    w: Math.max(1, Math.round(popup.width * at.scale)),
    h: Math.max(1, Math.round(popup.height * at.scale)),
  };
}

/** What the panel sends on its socket. Mirrors `BrowserMessage` in
 * `deckjack-protocol/src/stream.rs`. */
export type StreamInput =
  | { type: "pointer-move"; x: number; y: number }
  | { type: "pointer-button"; x: number; y: number; button: number; pressed: boolean }
  | { type: "scroll"; x: number; y: number; dx: number; dy: number }
  | { type: "key"; code: number; pressed: boolean }
  | { type: "pointer-leave" }
  | { type: "focus" };

export type StreamMessage =
  | { type: "input"; input: StreamInput }
  | { type: "resize"; width: number; height: number }
  | { type: "keyframe" };

/** The stream socket's URL for one window, on the page's own origin. */
export function streamUrl(location: { protocol: string; host: string }, window: string): string {
  const scheme = location.protocol === "https:" ? "wss:" : "ws:";
  return `${scheme}//${location.host}/stream?window=${encodeURIComponent(window)}`;
}

/** The close code the core uses when this device no longer holds the seat. */
export const CLOSE_SEAT = 4403;

/** Pictures waiting inside the decoder before deltas are dropped. */
export const MAX_DECODE_QUEUE = 4;

/** Decoder failures in a row, with no picture decoded between them,
 * before the stream is given up on as one this browser cannot decode. */
export const MAX_DECODE_FAILURES = 3;

export interface PictureDecoderOptions {
  /** Whether this browser can decode a configuration, asked once per codec
   * string before the first configure (`VideoDecoder.isConfigSupported`). */
  supported?: (config: VideoDecoderConfig) => Promise<boolean>;
  /** The stream cannot be decoded here: unsupported, or failing again and
   * again. Nothing more is decoded or asked for. */
  onFatal?: () => void;
  /** Run `fn` after `ms` (a seam for tests). */
  schedule?: (fn: () => void, ms: number) => void;
}

/**
 * Feeds pictures to a `VideoDecoder`, configuring it from each keyframe's
 * SPS and PPS, and dropping deltas until the first keyframe after a
 * (re)configure. Calls `onLost` when the decoder fails or falls behind, so
 * the caller can ask for a keyframe.
 *
 * Two loops it refuses to be part of: a decoder slower than the stream
 * (the socket stays drained, so the core never sees a slow viewer; here
 * deltas are dropped once `MAX_DECODE_QUEUE` wait, and one keyframe is
 * asked for), and a browser that cannot decode the stream at all (each
 * failure would ask for another keyframe forever; here the configuration
 * is checked first, failures back off, and after `MAX_DECODE_FAILURES` in
 * a row the stream is given up on through `onFatal`).
 */
export class PictureDecoder {
  private decoder: VideoDecoder | null = null;
  private codec: string | null = null;
  private description: Uint8Array | null = null;
  /** Deltas are skipped until the next keyframe: one was dropped. */
  private needsKey = false;
  private failures = 0;
  private fatal = false;
  /** Codec strings `supported` has said yes to, and the one being asked. */
  private readonly verified = new Set<string>();
  private checking: string | null = null;
  /** The keyframe that arrived while its configuration was being checked. */
  private waiting: Picture | null = null;

  /** Each picture's visible size, by timestamp, until its frame comes out:
   * the decoder may hold a few. */
  private readonly sizes = new Map<number, { width: number; height: number }>();

  constructor(
    private readonly output: (frame: VideoFrame, picture: { width: number; height: number }) => void,
    private readonly onLost: () => void,
    private readonly create: (init: VideoDecoderInit) => VideoDecoder = (init) => new VideoDecoder(init),
    private readonly options: PictureDecoderOptions = {},
  ) {}

  /** True once the stream has been given up on. */
  get failed(): boolean {
    return this.fatal;
  }

  private giveUp(): void {
    if (this.fatal) return;
    this.fatal = true;
    this.reset();
    this.options.onFatal?.();
  }

  private onDecoderError(): void {
    this.decoder = null;
    this.failures += 1;
    if (this.failures >= MAX_DECODE_FAILURES) {
      this.giveUp();
      return;
    }
    // The first failure asks again at once; each one after it waits twice
    // as long, so a decoder that keeps failing is not a keyframe loop.
    if (this.failures === 1) this.onLost();
    else (this.options.schedule ?? ((fn, ms) => setTimeout(fn, ms)))(() => {
      if (!this.fatal) this.onLost();
    }, 250 * 2 ** (this.failures - 2));
  }

  push(picture: Picture): void {
    if (this.fatal) return;
    if (picture.keyframe) {
      const config = avcConfig(picture.data);
      if (!config) return;
      const supported = this.options.supported;
      if (supported && !this.verified.has(config.codec)) {
        // Nothing is configured until the browser says it can decode this.
        this.waiting = picture;
        if (this.checking === config.codec) return;
        this.checking = config.codec;
        supported({ codec: config.codec, description: config.description })
          .catch(() => false)
          .then((ok) => {
            this.checking = null;
            if (!ok) {
              this.giveUp();
              return;
            }
            this.verified.add(config.codec);
            const stashed = this.waiting;
            this.waiting = null;
            if (stashed) this.push(stashed);
          });
        return;
      }
      if (!this.decoder || this.decoder.state === "closed" || !sameBytes(config.description, this.description)) {
        this.reset();
        this.decoder = this.create({
          output: (frame) => {
            this.failures = 0;
            const size = this.sizes.get(frame.timestamp) ?? {
              width: frame.displayWidth,
              height: frame.displayHeight,
            };
            this.sizes.delete(frame.timestamp);
            this.output(frame, size);
          },
          error: () => this.onDecoderError(),
        });
        this.decoder.configure({ codec: config.codec, description: config.description, optimizeForLatency: true });
        this.codec = config.codec;
        this.description = config.description;
      }
      this.needsKey = false;
    } else if (!this.decoder || this.needsKey) {
      return;
    } else if (this.decoder.decodeQueueSize > MAX_DECODE_QUEUE) {
      // Behind: this delta and every one up to the next keyframe are
      // dropped, and one keyframe is asked for.
      this.needsKey = true;
      this.onLost();
      return;
    }
    const timestamp = picture.timestamp * 1000;
    this.sizes.set(timestamp, { width: picture.width, height: picture.height });
    if (this.sizes.size > 64) this.sizes.delete(this.sizes.keys().next().value!);
    this.decoder!.decode(
      new EncodedVideoChunk({
        type: picture.keyframe ? "key" : "delta",
        timestamp,
        data: toAvc(picture.data),
      }),
    );
  }

  get configuredCodec(): string | null {
    return this.codec;
  }

  reset(): void {
    try {
      if (this.decoder && this.decoder.state !== "closed") this.decoder.close();
    } catch {
      // Closing a decoder that already failed is not an error worth having.
    }
    this.decoder = null;
    this.needsKey = false;
    this.sizes.clear();
  }
}

function sameBytes(a: Uint8Array, b: Uint8Array | null): boolean {
  if (!b || a.length !== b.length) return false;
  for (let i = 0; i < a.length; i++) if (a[i] !== b[i]) return false;
  return true;
}
