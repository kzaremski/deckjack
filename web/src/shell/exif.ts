/**
 * Dimensions and EXIF from the head of a JPEG, in the browser.
 *
 * # Why client-side and not a command
 *
 * The bytes are already reachable: Quick Look asks the content route for the
 * first {@link EXIF_HEAD_BYTES} with a `Range` header, and a JPEG's EXIF block
 * is required to sit in the first 64 KiB (it is an APP1 segment, whose length
 * field is 16 bits). Parsing it here is a few hundred lines with no
 * dependency, keeps the core from growing an image-format parser that every
 * malformed photograph is fed to, and works the same for any node. A command
 * would buy nothing but a round trip.
 *
 * Every read is bounds-checked: EXIF is attacker-controlled, and a broken
 * offset yields a missing field, never an exception out of the preview.
 */

/** How much of a JPEG to fetch for its metadata. An APP1 segment is at most
 * 64 KiB and usually comes first; the rest is margin for an APP0 before it. */
export const EXIF_HEAD_BYTES = 128 * 1024;

export interface Exif {
  make?: string;
  model?: string;
  lens?: string;
  /** "2024-03-15 13:45:30", camera-local time, as the camera recorded it. */
  taken?: string;
  /** "1/250 s" or "2.5 s". */
  exposure?: string;
  /** "f/2.8". */
  fNumber?: string;
  iso?: number;
  /** "50 mm". */
  focalLength?: string;
  /** 1-8, the EXIF orientation code. Browsers apply it to `<img>` already. */
  orientation?: number;
  gps?: { latitude: number; longitude: number };
}

export interface JpegMetadata {
  width?: number;
  height?: number;
  exif: Exif;
}

export function readJpegMetadata(bytes: Uint8Array): JpegMetadata {
  const out: JpegMetadata = { exif: {} };
  if (bytes.length < 4 || bytes[0] !== 0xff || bytes[1] !== 0xd8) return out;
  const view = new DataView(bytes.buffer, bytes.byteOffset, bytes.byteLength);

  let p = 2;
  while (p + 4 <= bytes.length) {
    if (bytes[p] !== 0xff) break;
    const marker = bytes[p + 1]!;
    // Fill bytes and standalone markers carry no length.
    if (marker === 0xff) {
      p += 1;
      continue;
    }
    if (marker === 0xd8 || (marker >= 0xd0 && marker <= 0xd7) || marker === 0x01) {
      p += 2;
      continue;
    }
    if (marker === 0xda || marker === 0xd9) break; // image data or end
    const len = view.getUint16(p + 2);
    if (len < 2) break;
    const body = p + 4;
    const end = Math.min(p + 2 + len, bytes.length);

    if (marker === 0xe1 && end - body > 6 && ascii(bytes, body, 6) === "Exif\0\0") {
      try {
        out.exif = readTiff(bytes.subarray(body + 6, end));
      } catch {
        // A malformed block is no metadata, not a broken preview.
      }
    } else if (isStartOfFrame(marker) && end - body >= 5) {
      out.height = view.getUint16(body + 1);
      out.width = view.getUint16(body + 3);
    }
    p += 2 + len;
  }
  return out;
}

function isStartOfFrame(m: number): boolean {
  return m >= 0xc0 && m <= 0xcf && m !== 0xc4 && m !== 0xc8 && m !== 0xcc;
}

function ascii(bytes: Uint8Array, at: number, n: number): string {
  let s = "";
  for (let k = at; k < at + n && k < bytes.length; k += 1) s += String.fromCharCode(bytes[k]!);
  return s;
}

const TYPE_SIZE: Record<number, number> = { 1: 1, 2: 1, 3: 2, 4: 4, 5: 8, 7: 1, 9: 4, 10: 8 };

interface Field {
  type: number;
  count: number;
  /** Where the value starts, inside the TIFF block. */
  at: number;
}

function readTiff(t: Uint8Array): Exif {
  if (t.length < 8) return {};
  const little = t[0] === 0x49 && t[1] === 0x49;
  if (!little && !(t[0] === 0x4d && t[1] === 0x4d)) return {};
  const view = new DataView(t.buffer, t.byteOffset, t.byteLength);
  const u16 = (at: number) => (at + 2 <= t.length ? view.getUint16(at, little) : undefined);
  const u32 = (at: number) => (at + 4 <= t.length ? view.getUint32(at, little) : undefined);
  if (u16(2) !== 42) return {};

  const seen = new Set<number>();
  function ifd(offset: number | undefined): Map<number, Field> {
    const fields = new Map<number, Field>();
    // An IFD that points back at one already read would loop forever.
    if (offset === undefined || seen.has(offset)) return fields;
    seen.add(offset);
    const count = u16(offset);
    if (count === undefined) return fields;
    for (let n = 0; n < Math.min(count, 512); n += 1) {
      const e = offset + 2 + n * 12;
      const tag = u16(e);
      const type = u16(e + 2);
      const items = u32(e + 4);
      if (tag === undefined || type === undefined || items === undefined) break;
      const size = (TYPE_SIZE[type] ?? 0) * items;
      if (size === 0) continue;
      const at = size <= 4 ? e + 8 : u32(e + 8);
      if (at === undefined || at + size > t.length) continue;
      fields.set(tag, { type, count: items, at });
    }
    return fields;
  }

  const str = (f: Field | undefined) => {
    if (!f || f.type !== 2) return undefined;
    const s = ascii(t, f.at, f.count).replace(/\0[\s\S]*$/, "").trim();
    return s || undefined;
  };
  const int = (f: Field | undefined) => {
    if (!f) return undefined;
    if (f.type === 3) return u16(f.at);
    if (f.type === 4) return u32(f.at);
    return undefined;
  };
  const rational = (f: Field | undefined, index = 0) => {
    if (!f || (f.type !== 5 && f.type !== 10) || index >= f.count) return undefined;
    const at = f.at + index * 8;
    const num = f.type === 5 ? u32(at) : view.getInt32(at, little);
    const den = f.type === 5 ? u32(at + 4) : view.getInt32(at + 4, little);
    if (num === undefined || den === undefined || den === 0) return undefined;
    return { num, den, value: num / den };
  };

  const root = ifd(u32(4));
  const exifIfd = ifd(int(root.get(0x8769)));
  const gpsIfd = ifd(int(root.get(0x8825)));

  const out: Exif = {};
  const make = str(root.get(0x010f));
  if (make) out.make = make;
  const model = str(root.get(0x0110));
  if (model) out.model = model;
  const orientation = int(root.get(0x0112));
  if (orientation && orientation >= 1 && orientation <= 8) out.orientation = orientation;

  const taken = str(exifIfd.get(0x9003)) ?? str(root.get(0x0132));
  if (taken) out.taken = taken.replace(/^(\d{4}):(\d{2}):(\d{2})/, "$1-$2-$3");

  const exposure = rational(exifIfd.get(0x829a));
  if (exposure) {
    out.exposure =
      exposure.value >= 1 || exposure.num === 0
        ? `${round(exposure.value)} s`
        : `1/${Math.round(exposure.den / exposure.num)} s`;
  }
  const f = rational(exifIfd.get(0x829d));
  if (f) out.fNumber = `f/${round(f.value)}`;
  const iso = int(exifIfd.get(0x8827));
  if (iso) out.iso = iso;
  const focal = rational(exifIfd.get(0x920a));
  if (focal) out.focalLength = `${round(focal.value)} mm`;
  const lens = str(exifIfd.get(0xa434));
  if (lens) out.lens = lens;

  const lat = degrees(gpsIfd.get(0x0002), rational);
  const lon = degrees(gpsIfd.get(0x0004), rational);
  if (lat !== undefined && lon !== undefined) {
    out.gps = {
      latitude: str(gpsIfd.get(0x0001)) === "S" ? -lat : lat,
      longitude: str(gpsIfd.get(0x0003)) === "W" ? -lon : lon,
    };
  }
  return out;
}

function degrees(
  f: Field | undefined,
  rational: (f: Field | undefined, i?: number) => { value: number } | undefined,
): number | undefined {
  const d = rational(f, 0);
  const m = rational(f, 1);
  const s = rational(f, 2);
  if (!d) return undefined;
  return d.value + (m?.value ?? 0) / 60 + (s?.value ?? 0) / 3600;
}

function round(n: number): string {
  return String(Math.round(n * 10) / 10);
}
