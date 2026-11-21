import { readFileSync } from "node:fs";
import { describe, expect, it } from "vitest";

import { OUTPUT, bundleWithImports } from "../../scripts/build-editor-vendor.mjs";

/**
 * The Editor's CodeMirror is a checked-in build output (see the script for
 * why). This is what keeps it honest: the file in the repository must be
 * exactly what the entry and the locked package versions produce today, so a
 * version bump or an entry change that was not rebuilt fails here instead of
 * shipping a bundle nobody can reproduce.
 */
describe("the vendored editor bundle", () => {
  it("is what the build produces from the pinned sources", async () => {
    const { text } = await bundleWithImports();
    const onDisk = readFileSync(OUTPUT, "utf8");
    expect(onDisk.length, "run npm run build:editor-vendor").toBe(text.length);
    expect(onDisk === text, "run npm run build:editor-vendor").toBe(true);
  }, 30_000);

  it("is a self-contained ES module: no imports left to resolve at runtime", async () => {
    const { text, imports } = await bundleWithImports();
    expect(imports).toEqual([]);
    expect(text).toMatch(/export\s*\{/);
  }, 30_000);
});
