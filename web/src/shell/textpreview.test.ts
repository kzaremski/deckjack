import { describe, expect, it } from "vitest";

import { decodePreview, TEXT_PREVIEW_BYTES } from "./textpreview.ts";

/**
 * Turning the first bytes of a file into something to read.
 *
 * Quick Look fetches only the head of a text file -- a 300 MB log is not
 * pulled to a phone to show its first screen -- so the decoder has to be
 * honest about two things: that it is showing part of the file, and that a
 * file with an innocent extension can still be binary.
 */

const utf8 = (s: string) => new TextEncoder().encode(s);

describe("text", () => {
  it("is shown as it is", () => {
    const p = decodePreview(utf8("fn main() {\n    println!(\"hi\");\n}\n"), 34);
    expect(p.kind).toBe("text");
    if (p.kind !== "text") return;
    expect(p.text).toBe('fn main() {\n    println!("hi");\n}\n');
    expect(p.truncated).toBe(false);
  });

  it("says when it is only the start of the file", () => {
    const head = utf8("a".repeat(100));
    const p = decodePreview(head, 1_000_000);
    expect(p.kind === "text" && p.truncated, "a partial file was presented as whole").toBe(true);
  });

  it("does not end in a broken character where the fetch cut a letter in half", () => {
    // "é" is two bytes in UTF-8. A range that ends between them would show a
    // replacement character at the end of the preview, which reads as a
    // corrupt file.
    const bytes = utf8("café");
    const cut = bytes.slice(0, bytes.length - 1);
    const p = decodePreview(cut, 100);
    expect(p.kind).toBe("text");
    if (p.kind !== "text") return;
    expect(p.text).toBe("caf");
    expect(p.text).not.toContain("�");
  });

  it("reads a UTF-16 file by its byte-order mark", () => {
    const bytes = new Uint8Array([0xff, 0xfe, 0x68, 0x00, 0x69, 0x00]);
    const p = decodePreview(bytes, bytes.length);
    expect(p.kind === "text" && p.text).toBe("hi");
  });

  it("drops a UTF-8 byte-order mark rather than showing it", () => {
    const bytes = new Uint8Array([0xef, 0xbb, 0xbf, ...utf8("x")]);
    const p = decodePreview(bytes, bytes.length);
    expect(p.kind === "text" && p.text).toBe("x");
  });

  it("asks for a bounded head, not the whole file", () => {
    expect(TEXT_PREVIEW_BYTES).toBeGreaterThanOrEqual(64 * 1024);
    expect(TEXT_PREVIEW_BYTES).toBeLessThanOrEqual(1024 * 1024);
  });
});

describe("binary", () => {
  it("is recognized by a NUL byte, whatever the file is called", () => {
    // Mostly printable, one NUL: a SQLite page or a protobuf looks like this,
    // and so does an object file once past its header.
    const bytes = new Uint8Array([...utf8("SQLite format 3"), 0x00, ...utf8("tables and things")]);
    expect(decodePreview(bytes, bytes.length).kind).toBe("binary");
  });

  it("is recognized when it is mostly control characters", () => {
    const bytes = new Uint8Array(200).map((_, i) => (i % 3 === 0 ? 0x01 : 0x41));
    expect(decodePreview(bytes, 200).kind).toBe("binary");
  });

  it("is not claimed for text that merely has some odd bytes", () => {
    // Latin-1 accents in an old config file are invalid UTF-8 but plainly
    // text. Refusing it would hide a readable file.
    const bytes = new Uint8Array([...utf8("name = Jos"), 0xe9, ...utf8("\nport = 80\n")]);
    const p = decodePreview(bytes, bytes.length);
    expect(p.kind).toBe("text");
  });

  it("is not claimed for an empty file", () => {
    const p = decodePreview(new Uint8Array(0), 0);
    expect(p.kind === "text" && p.text).toBe("");
  });
});
