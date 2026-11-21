import { describe, expect, it } from "vitest";

import { readJpegMetadata } from "./exif.ts";

/**
 * EXIF from the head of a JPEG.
 *
 * The photographs are built here entry by entry, in both byte orders, so each
 * test states the facts it expects back. Cameras write big-endian (Nikon,
 * many Canon bodies) and little-endian (phones) about equally; a parser that
 * only ever met one of them reads nonsense from the other.
 */

type Entry =
  | { tag: number; type: "ascii"; value: string }
  | { tag: number; type: "short"; value: number }
  | { tag: number; type: "long"; value: number }
  | { tag: number; type: "rational"; value: Array<[number, number]> }
  | { tag: number; type: "ifd"; entries: Entry[] };

/** A TIFF structure with IFD0 = `entries`, sub-IFDs laid out after. */
function tiff(entries: Entry[], little: boolean): Uint8Array {
  const buf = new ArrayBuffer(4096);
  const view = new DataView(buf);
  const u8 = new Uint8Array(buf);
  u8[0] = little ? 0x49 : 0x4d;
  u8[1] = u8[0];
  view.setUint16(2, 42, little);
  view.setUint32(4, 8, little);
  let free = 8;

  function writeIfd(list: Entry[]): number {
    const at = free;
    free += 2 + list.length * 12 + 4;
    view.setUint16(at, list.length, little);
    list.forEach((e, n) => {
      const p = at + 2 + n * 12;
      view.setUint16(p, e.tag, little);
      if (e.type === "ascii") {
        const bytes = new TextEncoder().encode(e.value + "\0");
        view.setUint16(p + 2, 2, little);
        view.setUint32(p + 4, bytes.length, little);
        if (bytes.length <= 4) u8.set(bytes, p + 8);
        else {
          view.setUint32(p + 8, free, little);
          u8.set(bytes, free);
          free += bytes.length;
        }
      } else if (e.type === "short") {
        view.setUint16(p + 2, 3, little);
        view.setUint32(p + 4, 1, little);
        view.setUint16(p + 8, e.value, little);
      } else if (e.type === "long") {
        view.setUint16(p + 2, 4, little);
        view.setUint32(p + 4, 1, little);
        view.setUint32(p + 8, e.value, little);
      } else if (e.type === "rational") {
        view.setUint16(p + 2, 5, little);
        view.setUint32(p + 4, e.value.length, little);
        view.setUint32(p + 8, free, little);
        for (const [num, den] of e.value) {
          view.setUint32(free, num, little);
          view.setUint32(free + 4, den, little);
          free += 8;
        }
      } else {
        view.setUint16(p + 2, 4, little);
        view.setUint32(p + 4, 1, little);
        // Sub-IFD written later; patch the pointer once it exists.
        const sub = e.entries;
        queue.push(() => view.setUint32(p + 8, writeIfd(sub), little));
      }
    });
    view.setUint32(at + 2 + list.length * 12, 0, little);
    return at;
  }
  const queue: Array<() => void> = [];
  writeIfd(entries);
  while (queue.length) queue.shift()!();
  return u8.slice(0, free);
}

function jpeg(exif: Uint8Array | null, size = { w: 6000, h: 4000 }): Uint8Array {
  const parts: number[] = [0xff, 0xd8];
  if (exif) {
    const body = [...new TextEncoder().encode("Exif\0\0"), ...exif];
    parts.push(0xff, 0xe1, ((body.length + 2) >> 8) & 0xff, (body.length + 2) & 0xff, ...body);
  }
  // A baseline SOF0: precision, height, width, components.
  parts.push(0xff, 0xc0, 0x00, 0x11, 0x08, size.h >> 8, size.h & 0xff, size.w >> 8, size.w & 0xff, 0x03);
  parts.push(...new Array(9).fill(0));
  parts.push(0xff, 0xda, 0x00, 0x02);
  return new Uint8Array(parts);
}

const camera = (little: boolean) =>
  tiff(
    [
      { tag: 0x010f, type: "ascii", value: "NIKON CORPORATION" },
      { tag: 0x0110, type: "ascii", value: "NIKON Z 6" },
      { tag: 0x0112, type: "short", value: 6 },
      {
        tag: 0x8769,
        type: "ifd",
        entries: [
          { tag: 0x829a, type: "rational", value: [[1, 250]] },
          { tag: 0x829d, type: "rational", value: [[28, 10]] },
          { tag: 0x8827, type: "short", value: 400 },
          { tag: 0x9003, type: "ascii", value: "2024:03:15 13:45:30" },
          { tag: 0x920a, type: "rational", value: [[50, 1]] },
          { tag: 0xa434, type: "ascii", value: "NIKKOR Z 50mm f/1.8 S" },
        ],
      },
      {
        tag: 0x8825,
        type: "ifd",
        entries: [
          { tag: 0x0001, type: "ascii", value: "N" },
          { tag: 0x0002, type: "rational", value: [[47, 1], [36, 1], [3000, 100]] },
          { tag: 0x0003, type: "ascii", value: "W" },
          { tag: 0x0004, type: "rational", value: [[122, 1], [19, 1], [1200, 100]] },
        ],
      },
    ],
    little,
  );

describe("a photograph's metadata", () => {
  for (const little of [true, false]) {
    it(`is read from a ${little ? "little" : "big"}-endian EXIF block`, () => {
      const m = readJpegMetadata(jpeg(camera(little)));
      expect(m.width).toBe(6000);
      expect(m.height).toBe(4000);
      expect(m.exif.make).toBe("NIKON CORPORATION");
      expect(m.exif.model).toBe("NIKON Z 6");
      expect(m.exif.orientation).toBe(6);
      expect(m.exif.exposure).toBe("1/250 s");
      expect(m.exif.fNumber).toBe("f/2.8");
      expect(m.exif.iso).toBe(400);
      expect(m.exif.focalLength).toBe("50 mm");
      expect(m.exif.lens).toBe("NIKKOR Z 50mm f/1.8 S");
      expect(m.exif.taken).toBe("2024-03-15 13:45:30");
      expect(m.exif.gps?.latitude).toBeCloseTo(47.6083, 3);
      expect(m.exif.gps?.longitude).toBeCloseTo(-122.3200, 3);
    });
  }

  it("reports dimensions for a JPEG with no EXIF at all", () => {
    const m = readJpegMetadata(jpeg(null, { w: 640, h: 480 }));
    expect([m.width, m.height]).toEqual([640, 480]);
    expect(m.exif).toEqual({});
  });

  it("gives a long exposure in seconds, not as a fraction", () => {
    const t = tiff([{ tag: 0x8769, type: "ifd", entries: [{ tag: 0x829a, type: "rational", value: [[25, 10]] }] }], true);
    expect(readJpegMetadata(jpeg(t)).exif.exposure).toBe("2.5 s");
  });
});

describe("a file that is not what it claims", () => {
  it("is not a JPEG, and says nothing rather than throwing", () => {
    const m = readJpegMetadata(new TextEncoder().encode("not an image at all"));
    expect(m).toEqual({ exif: {} });
  });

  it("survives offsets that point outside the data", () => {
    // EXIF is attacker-controlled; an offset past the end must not throw out
    // of the preview, it must just yield nothing for that field.
    const t = camera(true);
    const view = new DataView(t.buffer);
    // Point IFD0's first entry (Make) at a far-away offset.
    view.setUint32(8 + 2 + 8, 0x7fff_fff0, true);
    const m = readJpegMetadata(jpeg(t));
    expect(m.exif.make).toBeUndefined();
    expect(m.exif.model).toBe("NIKON Z 6");
  });

  it("drops a field that runs off the end rather than returning part of it", () => {
    const t = camera(true);
    const view = new DataView(t.buffer);
    // Make's 18 bytes now start 4 bytes before the end of the block.
    view.setUint32(8 + 2 + 8, t.length - 4, true);
    expect(readJpegMetadata(jpeg(t)).exif.make).toBeUndefined();
  });

  it("survives a truncated head", () => {
    const whole = jpeg(camera(false));
    for (const cut of [3, 10, 30, 60, 100]) {
      expect(() => readJpegMetadata(whole.slice(0, cut))).not.toThrow();
    }
  });

  it("survives an IFD that points at itself", () => {
    const t = tiff([{ tag: 0x8769, type: "long", value: 8 }], true);
    expect(() => readJpegMetadata(jpeg(t))).not.toThrow();
  });
});
