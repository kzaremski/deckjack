import { readFileSync } from "node:fs";
import { resolve } from "node:path";

import { describe, expect, it } from "vitest";

import {
  EVDEV,
  MAX_DECODE_FAILURES,
  MAX_DECODE_QUEUE,
  PictureDecoder,
  buttonChanges,
  releasedWithMeta,
  toWindowPointClamped,
  avcConfig,
  evdevFor,
  nalUnits,
  parsePicture,
  parseStreamMeta,
  popupRect,
  streamUrl,
  toAvc,
  toWindowPoint,
  wheelPixels,
  type Picture,
} from "./stream.ts";

/**
 * The browser's half of a streamed native app, without a browser: how a
 * picture is read off the socket and handed to WebCodecs, and how input is
 * turned into what the compositor expects.
 *
 * The pictures are the compositor's own encoder output: the same sample the
 * browser decode spike (S3, `e2e/spike/stream.e2e.ts`) decodes in Chromium
 * and WebKit, framed as the stream socket frames them.
 */

function sample(): ArrayBuffer[] {
  const bytes = readFileSync(resolve(__dirname, "../../e2e/spike/pages/stream/text-320x240.bin"));
  const out: ArrayBuffer[] = [];
  for (let at = 0; at < bytes.length; ) {
    const len = bytes.readUInt32LE(at);
    out.push(bytes.buffer.slice(bytes.byteOffset + at + 4, bytes.byteOffset + at + 4 + len));
    at += 4 + len;
  }
  return out;
}

describe("a picture off the stream socket", () => {
  it("carries its keyframe flag, visible size and H.264", () => {
    const [first, second] = sample();
    const key = parsePicture(first!)!;
    expect(key.keyframe).toBe(true);
    expect([key.width, key.height]).toEqual([320, 240]);
    expect([...key.data.subarray(0, 4)]).toEqual([0, 0, 0, 1]);
    expect(parsePicture(second!)!.keyframe).toBe(false);
  });

  it("is refused when it is too short to hold a header and a start code", () => {
    expect(parsePicture(new ArrayBuffer(12))).toBeNull();
  });
});

describe("handing the bitstream to WebCodecs as AVC", () => {
  it("builds the codec string and avcC record from the keyframe's SPS and PPS", () => {
    const key = parsePicture(sample()[0]!)!;
    const units = nalUnits(key.data);
    const sps = units.find((u) => (u[0]! & 0x1f) === 7)!;
    const pps = units.find((u) => (u[0]! & 0x1f) === 8)!;
    const config = avcConfig(key.data)!;
    // Constrained Baseline (0x42, constraints 0xc0), level 2.1 for 320x240.
    expect(config.codec).toBe("avc1.42c015");
    const d = config.description;
    expect([...d.subarray(0, 6)]).toEqual([1, 0x42, 0xc0, 0x15, 0xff, 0xe1]);
    expect((d[6]! << 8) | d[7]!).toBe(sps.length);
    expect([...d.subarray(8, 8 + sps.length)]).toEqual([...sps]);
    expect(d[8 + sps.length]).toBe(1);
    expect([...d.subarray(11 + sps.length)]).toEqual([...pps]);
  });

  it("prefixes each slice with its length and leaves the parameter sets out", () => {
    const key = parsePicture(sample()[0]!)!;
    const avc = toAvc(key.data);
    const slices = nalUnits(key.data).filter((u) => ![7, 8].includes(u[0]! & 0x1f));
    expect(slices.length).toBeGreaterThan(0);
    let at = 0;
    for (const slice of slices) {
      const len = new DataView(avc.buffer, avc.byteOffset + at, 4).getUint32(0);
      expect(len).toBe(slice.length);
      expect([...avc.subarray(at + 4, at + 4 + len)]).toEqual([...slice]);
      at += 4 + len;
    }
    expect(at).toBe(avc.length);
  });

  it("says a picture with no parameter sets cannot configure a decoder", () => {
    expect(avcConfig(parsePicture(sample()[1]!)!.data)).toBeNull();
  });
});

/** A decoder that records what it was given, standing in for WebCodecs. */
function fakeDecoder() {
  const log: string[] = [];
  let init: VideoDecoderInit | null = null;
  const decoder = {
    state: "configured",
    decodeQueueSize: 0,
    configure(c: VideoDecoderConfig) {
      log.push(`configure ${c.codec} ${c.description ? "avcC" : "annexb"}`);
    },
    decode(chunk: EncodedVideoChunk) {
      log.push(`decode ${chunk.type}`);
    },
    close() {
      log.push("close");
    },
  };
  return {
    log,
    decoder,
    fail: () => init!.error(new DOMException("broken", "EncodingError")),
    emit: () => init!.output({ timestamp: -1, displayWidth: 1, displayHeight: 1 } as unknown as VideoFrame),
    create: (i: VideoDecoderInit) => {
      init = i;
      return decoder as unknown as VideoDecoder;
    },
  };
}

describe("feeding the decoder", () => {
  // jsdom has no WebCodecs; the chunk is only carried, never decoded here.
  globalThis.EncodedVideoChunk ??= class {
    type: string;
    constructor(init: { type: string }) {
      this.type = init.type;
    }
  } as unknown as typeof EncodedVideoChunk;

  const pictures = (): Picture[] => sample().map((b) => parsePicture(b)!);

  it("drops deltas until a keyframe has configured it, then decodes in order", () => {
    const fake = fakeDecoder();
    const d = new PictureDecoder(() => {}, () => {}, fake.create);
    const [key, ...deltas] = pictures();
    d.push(deltas[0]!);
    expect(fake.log, "a delta with nothing to apply it to").toEqual([]);
    d.push(key!);
    d.push(deltas[0]!);
    expect(fake.log).toEqual(["configure avc1.42c015 avcC", "decode key", "decode delta"]);
  });

  it("asks for a keyframe when the decoder fails, and waits for one", () => {
    const fake = fakeDecoder();
    let lost = 0;
    const d = new PictureDecoder(() => {}, () => lost++, fake.create);
    const [key, delta] = pictures();
    d.push(key!);
    fake.fail();
    expect(lost).toBe(1);
    fake.log.length = 0;
    d.push(delta!);
    expect(fake.log, "a delta after a failure decoded against nothing").toEqual([]);
    d.push(key!);
    expect(fake.log).toEqual(["configure avc1.42c015 avcC", "decode key"]);
  });

  it("drops deltas while the decoder is behind, asks once for a keyframe, and resumes at it", () => {
    const fake = fakeDecoder();
    let lost = 0;
    const d = new PictureDecoder(() => {}, () => lost++, fake.create);
    const [key, delta] = pictures();
    d.push(key!);
    d.push(delta!);
    fake.decoder.decodeQueueSize = MAX_DECODE_QUEUE;
    d.push(delta!);
    expect(lost, "at the bound it is not behind yet").toBe(0);
    fake.decoder.decodeQueueSize = MAX_DECODE_QUEUE + 1;
    fake.log.length = 0;
    d.push(delta!);
    d.push(delta!);
    expect(fake.log, "a delta was decoded while the decoder was behind").toEqual([]);
    expect(lost, "one keyframe is asked for, not one per delta").toBe(1);
    // Caught up: still nothing until the keyframe, then everything.
    fake.decoder.decodeQueueSize = 0;
    d.push(delta!);
    expect(fake.log).toEqual([]);
    d.push(key!);
    d.push(delta!);
    expect(fake.log).toEqual(["decode key", "decode delta"]);
  });

  it("backs off after a failure and gives up after failing again and again", () => {
    const fake = fakeDecoder();
    let lost = 0;
    let fatal = 0;
    const later: { fn: () => void; ms: number }[] = [];
    const d = new PictureDecoder(() => {}, () => lost++, fake.create, {
      onFatal: () => fatal++,
      schedule: (fn, ms) => later.push({ fn, ms }),
    });
    const [key] = pictures();
    d.push(key!);
    fake.fail();
    expect(lost, "the first failure asks at once").toBe(1);
    d.push(key!);
    fake.fail();
    expect(lost, "the second waits").toBe(1);
    expect(later.map((l) => l.ms)).toEqual([250]);
    later.shift()!.fn();
    expect(lost).toBe(2);
    // A picture decoded in between starts the count again.
    d.push(key!);
    fake.emit();
    for (let i = 0; i < MAX_DECODE_FAILURES - 1; i++) {
      fake.fail();
      d.push(key!);
    }
    expect(fatal, "failures with a decoded picture between them are not in a row").toBe(0);
    fake.fail();
    expect(fatal).toBe(1);
    expect(d.failed).toBe(true);
    fake.log.length = 0;
    d.push(key!);
    later.forEach((l) => l.fn());
    expect(fake.log, "a given-up stream decodes nothing more").toEqual([]);
  });

  it("asks the browser before configuring, and gives up when it says no", async () => {
    const fake = fakeDecoder();
    let fatal = 0;
    const asked: string[] = [];
    let answer = false;
    const d = new PictureDecoder(() => {}, () => {}, fake.create, {
      supported: async (c) => {
        asked.push(c.codec);
        return answer;
      },
      onFatal: () => fatal++,
    });
    const [key, delta] = pictures();
    d.push(key!);
    d.push(delta!);
    d.push(key!);
    await Promise.resolve();
    await Promise.resolve();
    expect(asked, "asked once per codec").toEqual(["avc1.42c015"]);
    expect(fake.log).toEqual([]);
    expect(fatal).toBe(1);

    const yes = fakeDecoder();
    answer = true;
    const ok = new PictureDecoder(() => {}, () => {}, yes.create, { supported: async () => answer });
    ok.push(key!);
    expect(yes.log, "nothing configured before the answer").toEqual([]);
    await Promise.resolve();
    await Promise.resolve();
    expect(yes.log, "the keyframe that waited is decoded").toEqual(["configure avc1.42c015 avcC", "decode key"]);
  });

  it("keeps one decoder across keyframes that carry the same parameters", () => {
    const fake = fakeDecoder();
    const d = new PictureDecoder(() => {}, () => {}, fake.create);
    const [key] = pictures();
    d.push(key!);
    d.push(key!);
    expect(fake.log.filter((l) => l.startsWith("configure"))).toHaveLength(1);
  });
});

describe("keys", () => {
  it("are evdev codes for the physical key, and an unknown key is not sent", () => {
    expect(evdevFor("KeyA")).toBe(30);
    expect(evdevFor("Enter")).toBe(28);
    expect(evdevFor("ShiftLeft")).toBe(42);
    expect(evdevFor("ArrowUp")).toBe(103);
    expect(evdevFor("MetaLeft")).toBe(125);
    expect(evdevFor("Unidentified")).toBeNull();
    // Not a property of the table's prototype.
    expect(evdevFor("toString")).toBeNull();
  });

  it("never maps two keys to one code", () => {
    const codes = Object.values(EVDEV);
    expect(new Set(codes).size).toBe(codes.length);
  });
});

describe("buttons and held keys", () => {
  it("are diffed from the buttons mask, so a chorded press or release is not lost", () => {
    expect(buttonChanges(0, 1)).toEqual([{ button: 0, pressed: true }]);
    // Left held, right pressed: a pointermove with buttons 3.
    expect(buttonChanges(1, 3)).toEqual([{ button: 2, pressed: true }]);
    // Left released while right stays down.
    expect(buttonChanges(3, 2)).toEqual([{ button: 0, pressed: false }]);
    expect(buttonChanges(2, 0)).toEqual([{ button: 2, pressed: false }]);
    expect(buttonChanges(0, 4 | 8 | 16)).toEqual([
      { button: 1, pressed: true },
      { button: 3, pressed: true },
      { button: 4, pressed: true },
    ]);
    // The pen eraser bit is not a mouse button.
    expect(buttonChanges(0, 32)).toEqual([]);
    expect(buttonChanges(5, 5)).toEqual([]);
  });

  it("releases every held key but the modifiers when Meta is released", () => {
    // Cmd (125) + Shift (42) + C (46) + V (47).
    expect(releasedWithMeta([125, 42, 46, 47])).toEqual([46, 47]);
    expect(releasedWithMeta([29, 54, 56, 97, 100, 126])).toEqual([]);
  });
});

describe("the pointer", () => {
  const panel = { left: 100, top: 50, width: 800, height: 400 };

  it("maps the panel to the window's logical pixels at scale 1", () => {
    expect(toWindowPoint(110, 60, panel, { width: 800, height: 400 })).toEqual({ x: 10, y: 10 });
  });

  it("accounts for a picture scaled down and letterboxed", () => {
    // A 1600x400 picture in an 800x400 panel: half size, centered vertically
    // with 100px bars above and below.
    expect(toWindowPoint(500, 250, panel, { width: 1600, height: 400 })).toEqual({ x: 800, y: 200 });
    expect(toWindowPoint(500, 120, panel, { width: 1600, height: 400 }), "on the bar").toBeNull();
  });

  it("holds a release or a drag outside the picture to its edge instead of dropping it", () => {
    const box = { left: 0, top: 0, width: 400, height: 300 };
    const picture = { width: 400, height: 300 };
    expect(toWindowPoint(450, -10, box, picture)).toBeNull();
    expect(toWindowPointClamped(450, -10, box, picture)).toEqual({ x: 399, y: 0 });
    expect(toWindowPointClamped(100, 50, box, picture)).toEqual({ x: 100, y: 50 });
    expect(toWindowPointClamped(-5, 900, box, picture)).toEqual({ x: 0, y: 299 });
    expect(toWindowPointClamped(1, 1, box, { width: 0, height: 0 })).toBeNull();
  });

  it("scales the wheel to pixels whatever unit the browser used", () => {
    expect(wheelPixels({ deltaX: 0, deltaY: 100, deltaMode: 0 })).toEqual({ dx: 0, dy: 100 });
    expect(wheelPixels({ deltaX: 0, deltaY: 3, deltaMode: 1 })).toEqual({ dx: 0, dy: 99 });
    expect(wheelPixels({ deltaX: 1, deltaY: 0, deltaMode: 2 })).toEqual({ dx: 800, dy: 0 });
  });
});

describe("a popup's window", () => {
  const meta = parseStreamMeta({
    app: "gimp",
    appTitle: "GIMP",
    kind: "popup",
    parent: "stream-1-1",
    width: 200,
    height: 300,
    x: 40,
    y: 25,
  })!;

  it("goes where the app put it, against its parent's picture below the title bar", () => {
    const parent = { x: 100, y: 80, w: 800, h: 634 };
    expect(popupRect(parent, 34, { width: 800, height: 600 }, meta)).toEqual({ x: 140, y: 139, w: 200, h: 300 });
  });

  it("scales with its parent's picture", () => {
    // The parent drawn at half size: a 1600x1200 picture in an 800x600 box.
    const parent = { x: 0, y: 0, w: 800, h: 634 };
    expect(popupRect(parent, 34, { width: 1600, height: 1200 }, meta)).toEqual({ x: 20, y: 47, w: 100, h: 150 });
  });

  it("is read from the window list, and anything that is not a stream is not one", () => {
    expect(meta.kind).toBe("popup");
    expect(meta.parent).toBe("stream-1-1");
    expect(parseStreamMeta(undefined)).toBeNull();
    expect(parseStreamMeta({ kind: "popup" })).toBeNull();
    expect(parseStreamMeta({ app: "x" })!.kind).toBe("toplevel");
  });
});

describe("the socket", () => {
  it("is on the page's own origin, with the window in the query", () => {
    expect(streamUrl({ protocol: "https:", host: "dj.example:443" }, "stream-1-2")).toBe(
      "wss://dj.example:443/stream?window=stream-1-2",
    );
    expect(streamUrl({ protocol: "http:", host: "127.0.0.1:8790" }, "a b")).toBe(
      "ws://127.0.0.1:8790/stream?window=a%20b",
    );
  });
});
