import { test as base, expect } from "@playwright/test";

import { type Spike, startSpike } from "./server.ts";

/**
 * Spike S3 of docs/lanes/8-stream.md: can the browser decode what
 * deckjack-stream sends? The samples in `pages/stream/` are the
 * compositor's own encoder output (`cargo run --release -p deckjack-stream
 * --example encode_bench -- --fixture <file> <w> <h>`), framed exactly as
 * the stream socket frames a picture: a keyframe, then nine pictures that
 * each move a red caret.
 *
 * Two bitstream formats, because the engines differ:
 *
 * - **AVC** (an avcC `description`, NAL units prefixed by their length):
 *   what the stream panel hands the decoder. Both engines decode it.
 * - **Annex B** (start codes, no description), what the encoder writes:
 *   Chromium decodes it; WebKit (the Linux build, GStreamer underneath)
 *   says the config is supported and then fails every chunk with
 *   `EncodingError`. That is why the panel converts.
 *
 * Asserted per engine, as found on 2026-11-22, so an engine update that
 * changes an answer the stream panel leans on fails here first.
 */

const test = base.extend<object, { spike: Spike }>({
  spike: [
    async ({}, use) => {
      const spike = await startSpike();
      await use(spike);
      await spike.close();
    },
    { scope: "worker" },
  ],
});

interface Decoded {
  engine: string;
  hasVideoDecoder: boolean;
  codec?: string;
  supported?: boolean;
  supportError?: string;
  decoded?: number;
  pictures?: number;
  error?: string | null;
  msMedian?: number | null;
  msMax?: number | null;
  frameSize?: number[];
  background?: number[];
  caret?: number[];
}

async function decode(
  page: import("@playwright/test").Page,
  spike: Spike,
  sample: string,
  avc: boolean,
  label: string,
  info: import("@playwright/test").TestInfo,
): Promise<Decoded> {
  await page.goto(`${spike.A}/stream/decode.html?avc=${avc ? 1 : 0}&sample=/stream/${sample}`);
  const result = (await page.evaluate("window.result")) as Decoded;
  await info.attach(`${label}.json`, { body: JSON.stringify(result, null, 2), contentType: "application/json" });
  console.log(`${label}: ${JSON.stringify(result)}`);
  return result;
}

for (const sample of ["text-320x240.bin", "text-1920x1080.bin"]) {
  test(`S3: WebCodecs decodes the compositor's H.264 as AVC (${sample})`, async ({ page, spike, browserName }, info) => {
    const result = await decode(page, spike, sample, true, `S3 ${browserName} avc ${sample}`, info);
    expect(result.hasVideoDecoder).toBe(true);
    expect(result.supported).toBe(true);
    expect(result.error ?? null).toBeNull();
    expect(result.decoded).toBe(result.pictures);
    // The picture is right, not merely decoded: the background is the
    // light gray it was encoded as, and the last caret is red.
    const [r, g, b] = result.background!;
    expect(Math.abs(r! - 244) + Math.abs(g! - 244) + Math.abs(b! - 244)).toBeLessThan(24);
    const [cr, cg, cb] = result.caret!;
    expect(cr!).toBeGreaterThan(150);
    expect(cg!).toBeLessThan(110);
    expect(cb!).toBeLessThan(110);
  });
}

test("S3: Annex B decodes in Chromium and fails in WebKit", async ({ page, spike, browserName }, info) => {
  const result = await decode(page, spike, "text-320x240.bin", false, `S3 ${browserName} annexb`, info);
  // Both say they support it; that answer is no use for choosing a format.
  expect(result.supported).toBe(true);
  if (browserName === "webkit") {
    expect(result.decoded).toBe(0);
    expect(result.error).toContain("EncodingError");
  } else {
    expect(result.error ?? null).toBeNull();
    expect(result.decoded).toBe(result.pictures);
  }
});
