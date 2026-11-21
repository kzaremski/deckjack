import { mkdtempSync, writeFileSync } from "node:fs";
import { createRequire } from "node:module";
import { tmpdir } from "node:os";
import { dirname, join } from "node:path";
import { pathToFileURL } from "node:url";

import { describe, expect, it } from "vitest";

import { openPdf } from "./pdf.ts";

/**
 * The vendored pdf.js, opened the way the viewer opens it.
 *
 * Not a test of pdf.js -- of the pinned version and the options this project
 * passes it. A pdf.js upgrade that renamed an option or changed how the
 * worker is found would otherwise surface only as a blank page on a phone.
 */

/** A PDF of `sizes.length` pages, each its own size, with correct offsets. */
function pdf(sizes: Array<[number, number]>): Uint8Array {
  const objects: string[] = [];
  const pageIds = sizes.map((_, n) => 3 + n);
  objects[1] = "<< /Type /Catalog /Pages 2 0 R >>";
  objects[2] = `<< /Type /Pages /Kids [${pageIds.map((id) => `${id} 0 R`).join(" ")}] /Count ${sizes.length} >>`;
  sizes.forEach(([w, h], n) => {
    objects[3 + n] = `<< /Type /Page /Parent 2 0 R /MediaBox [0 0 ${w} ${h}] >>`;
  });
  let out = "%PDF-1.4\n";
  const offsets: number[] = [];
  for (let id = 1; id < objects.length; id += 1) {
    offsets[id] = out.length;
    out += `${id} 0 obj\n${objects[id]}\nendobj\n`;
  }
  const xref = out.length;
  out += `xref\n0 ${objects.length}\n0000000000 65535 f \n`;
  for (let id = 1; id < objects.length; id += 1) {
    out += `${String(offsets[id]).padStart(10, "0")} 00000 n \n`;
  }
  out += `trailer\n<< /Size ${objects.length} /Root 1 0 R >>\nstartxref\n${xref}\n%%EOF\n`;
  return new TextEncoder().encode(out);
}

describe("the vendored pdf.js", () => {
  it("opens a document, counts its pages and measures each one", async () => {
    const dir = mkdtempSync(join(tmpdir(), "dj-pdf-"));
    const file = join(dir, "three.pdf");
    writeFileSync(file, pdf([[612, 792], [842, 595], [300, 300]]));

    const require = createRequire(import.meta.url);
    const worker = require.resolve("pdfjs-dist/legacy/build/pdf.worker.min.mjs");
    const data = `${dirname(dirname(dirname(worker)))}/`;

    const doc = await openPdf(file, {
      workerSrc: pathToFileURL(worker).href,
      dataUrl: data,
    });
    try {
      expect(doc.numPages).toBe(3);
      const second = await doc.getPage(2);
      const v = second.getViewport({ scale: 1 });
      expect([Math.round(v.width), Math.round(v.height)]).toEqual([842, 595]);
    } finally {
      await doc.destroy();
    }
  }, 20_000);
});
