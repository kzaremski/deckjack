import { writeFileSync } from "node:fs";
import { join } from "node:path";
import { crc32, deflateSync } from "node:zlib";

/**
 * The files every scratch node's working directory starts with.
 *
 * The image is a real, decodable PNG with a size the tests can check, so
 * "Quick Look showed the image" means the browser decoded bytes the core
 * served, not that an <img> element exists. The text has a line no part of
 * the shell would print on its own.
 */
export interface Seeded {
  text: { name: string; firstLine: string };
  image: { name: string; width: number; height: number };
}

export const SEEDED: Seeded = {
  text: { name: "hello.txt", firstLine: "Hello from the Deckjack e2e harness" },
  image: { name: "swatch.png", width: 24, height: 16 },
};

export function seed(work: string): Seeded {
  writeFileSync(join(work, SEEDED.text.name), `${SEEDED.text.firstLine}\nsecond line\n`);
  writeFileSync(join(work, SEEDED.image.name), png(SEEDED.image.width, SEEDED.image.height));
  return SEEDED;
}

/** An RGB PNG of the given size: a horizontal gradient, so it is not blank. */
export function png(width: number, height: number): Buffer {
  const row = 1 + width * 3;
  const raw = Buffer.alloc(row * height);
  for (let y = 0; y < height; y++) {
    raw[y * row] = 0; // filter: none
    for (let x = 0; x < width; x++) {
      const o = y * row + 1 + x * 3;
      raw[o] = Math.round((x / Math.max(1, width - 1)) * 255);
      raw[o + 1] = Math.round((y / Math.max(1, height - 1)) * 255);
      raw[o + 2] = 128;
    }
  }
  const ihdr = Buffer.alloc(13);
  ihdr.writeUInt32BE(width, 0);
  ihdr.writeUInt32BE(height, 4);
  ihdr[8] = 8; // bit depth
  ihdr[9] = 2; // color type: RGB
  return Buffer.concat([
    Buffer.from([0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a]),
    chunk("IHDR", ihdr),
    chunk("IDAT", deflateSync(raw)),
    chunk("IEND", Buffer.alloc(0)),
  ]);
}

function chunk(type: string, data: Buffer): Buffer {
  const head = Buffer.alloc(8);
  head.writeUInt32BE(data.length, 0);
  head.write(type, 4, "ascii");
  const crc = Buffer.alloc(4);
  crc.writeUInt32BE(crc32(Buffer.concat([head.subarray(4), data])), 0);
  return Buffer.concat([head, data, crc]);
}
