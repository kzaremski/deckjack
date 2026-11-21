/**
 * The head of a text file, decoded for reading.
 *
 * Quick Look asks the content route for the first {@link TEXT_PREVIEW_BYTES}
 * with a `Range` header and decodes them here. Nothing is executed and
 * nothing is interpreted: the result is shown as text in a `<pre>`, or as
 * Markdown through `markdown.ts`, which escapes everything it is given.
 */

/** How much of a text file a preview fetches. Enough for any source file a
 * person reads in a preview; a log of hundreds of megabytes shows its head and
 * says so. */
export const TEXT_PREVIEW_BYTES = 512 * 1024;

export type TextPreview =
  | {
      kind: "text";
      text: string;
      /** Only the start of the file is shown. */
      truncated: boolean;
      /** Some bytes were not valid in the encoding and were replaced. */
      lossy: boolean;
    }
  | { kind: "binary" };

/**
 * `bytes` is the head that arrived; `total` is the file's real length, from
 * the response's `Content-Range` (or the byte count when it was not ranged).
 */
export function decodePreview(bytes: Uint8Array, total: number): TextPreview {
  const truncated = total > bytes.length;

  // UTF-16 announces itself; without the mark, NUL bytes below would call it
  // binary, which for a Windows-exported CSV is wrong.
  if (bytes.length >= 2 && ((bytes[0] === 0xff && bytes[1] === 0xfe) || (bytes[0] === 0xfe && bytes[1] === 0xff))) {
    const le = bytes[0] === 0xff;
    const even = bytes.subarray(2, 2 + ((bytes.length - 2) & ~1));
    const text = new TextDecoder(le ? "utf-16le" : "utf-16be").decode(even);
    return { kind: "text", text, truncated, lossy: text.includes("�") };
  }

  if (looksBinary(bytes)) return { kind: "binary" };

  let body = bytes;
  if (body[0] === 0xef && body[1] === 0xbb && body[2] === 0xbf) body = body.subarray(3);
  if (truncated) body = body.subarray(0, completeUtf8Length(body));

  const text = new TextDecoder("utf-8").decode(body);
  return { kind: "text", text, truncated, lossy: text.includes("�") };
}

/**
 * The heuristic every pager uses: a NUL in the first few kilobytes, or a
 * high share of control characters that are not whitespace.
 */
function looksBinary(bytes: Uint8Array): boolean {
  const sample = bytes.subarray(0, 8192);
  if (sample.length === 0) return false;
  let control = 0;
  for (const b of sample) {
    if (b === 0) return true;
    if (b < 0x20 && b !== 0x09 && b !== 0x0a && b !== 0x0d && b !== 0x0c && b !== 0x1b) control += 1;
  }
  return control / sample.length > 0.1;
}

/**
 * The length of `bytes` without a trailing, incomplete UTF-8 sequence -- the
 * one a byte range cut in half.
 */
function completeUtf8Length(bytes: Uint8Array): number {
  const n = bytes.length;
  // Walk back over at most three continuation bytes to the lead byte.
  for (let back = 1; back <= Math.min(4, n); back += 1) {
    const b = bytes[n - back]!;
    if ((b & 0xc0) === 0x80) continue; // continuation
    const need = b >= 0xf0 ? 4 : b >= 0xe0 ? 3 : b >= 0xc0 ? 2 : 1;
    return need > back ? n - back : n;
  }
  return n;
}
