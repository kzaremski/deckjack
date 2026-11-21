/**
 * PDF for Quick Look, through a vendored pdf.js.
 *
 * # Why not the browser's own viewer
 *
 * It was the first plan and it does not survive contact with the sandbox.
 * The content route sends `Content-Security-Policy: sandbox` with every file
 * -- the only thing that makes serving the operator's own HTML safe -- and a
 * sandboxed document cannot host Chromium's PDF plugin: it shows "This page
 * has been blocked" instead. Firefox's viewer and Safari's differ again, and
 * none of them can be told to page, zoom or report a page count from here.
 * Loosening the sandbox for PDFs alone would put a PDF's own JavaScript (which
 * the format has) back in reach of a viewer we do not control.
 *
 * pdf.js renders into a `<canvas>` we own, runs the PDF's script never
 * (scripting is a separate component this file does not load), and talks to
 * the content route with `Range` requests, so a 40-page PDF on a phone fetches
 * the pages it shows rather than the whole file.
 *
 * # Offline, and only when needed
 *
 * The library, its worker and the data it loads at run time (character maps,
 * the standard fonts, the image decoders) are bundled into the shell and
 * served by the core: no CDN, nothing fetched from anywhere but this node.
 * `vite.config.ts` copies the data under `/pdfjs/`. The library itself is a
 * dynamic import, so its half a megabyte is paid the first time a PDF is
 * opened, not on every load of the shell.
 *
 * The legacy build, deliberately: it carries the polyfills that let it run on
 * the older Safari a phone may be stuck on, which is the device this viewer
 * most has to work on.
 */

import workerUrl from "pdfjs-dist/legacy/build/pdf.worker.min.mjs?url";

/** The few calls the viewer makes, so a test can stand in for pdf.js. */
export interface PdfPage {
  getViewport(options: { scale: number }): { width: number; height: number };
  render(options: {
    canvas: HTMLCanvasElement;
    canvasContext: CanvasRenderingContext2D;
    viewport: unknown;
  }): { promise: Promise<void>; cancel(): void };
  cleanup?(): void;
}

export interface PdfDocument {
  numPages: number;
  getPage(n: number): Promise<PdfPage>;
  destroy(): Promise<void>;
}

/** Where `vite.config.ts` puts pdf.js's run-time data. */
export const PDFJS_DATA = "/pdfjs/";

/** How much one range request asks for while paging through a PDF. */
const RANGE_CHUNK = 256 * 1024;

/**
 * Open a PDF from a same-origin URL (the content route).
 *
 * `workerSrc` is the worker script's address; the default is the bundled
 * one. A test running under Node passes the file's own path.
 */
export async function openPdf(
  url: string,
  options: { workerSrc?: string; dataUrl?: string } = {},
): Promise<PdfDocument> {
  const pdfjs = await import("pdfjs-dist/legacy/build/pdf.mjs");
  pdfjs.GlobalWorkerOptions.workerSrc = options.workerSrc ?? workerUrl;
  const data = options.dataUrl ?? PDFJS_DATA;
  const task = pdfjs.getDocument({
    url,
    // Fetch the pages being looked at, not the whole file in the background.
    disableAutoFetch: true,
    rangeChunkSize: RANGE_CHUNK,
    // XFA forms are a second renderer with its own script engine; a preview
    // shows the static page.
    enableXfa: false,
    cMapUrl: `${data}cmaps/`,
    cMapPacked: true,
    standardFontDataUrl: `${data}standard_fonts/`,
    wasmUrl: `${data}wasm/`,
    iccUrl: `${data}iccs/`,
  });
  const doc = await task.promise;
  return {
    numPages: doc.numPages,
    getPage: async (n) => (await doc.getPage(n)) as unknown as PdfPage,
    // Destroying the loading task, not the document: in pdf.js 6 that is
    // what tears down the worker and aborts outstanding range requests.
    destroy: () => task.destroy(),
  };
}

/**
 * Render page `n` of `doc` into `canvas`, fitted to `width` CSS pixels and
 * drawn at the device's pixel density so text is sharp on a phone.
 *
 * Returns a cancel function: paging quickly must not leave a stale render
 * drawing over the page the operator moved to.
 */
export function renderPage(
  doc: PdfDocument,
  n: number,
  canvas: HTMLCanvasElement,
  width: number,
  zoom: number,
  pixelRatio: number,
): { done: Promise<{ width: number; height: number }>; cancel(): void } {
  let cancelled = false;
  let task: { cancel(): void } | null = null;
  const done = (async () => {
    const page = await doc.getPage(n);
    if (cancelled) throw new Error("cancelled");
    const unit = page.getViewport({ scale: 1 });
    const cssScale = (Math.max(width, 1) / unit.width) * zoom;
    // Capped so a deep zoom on a large page cannot ask for a canvas bigger
    // than the browser will allocate (and fail silently with a blank page).
    const backing = Math.min(cssScale * pixelRatio, 8192 / Math.max(unit.width, unit.height));
    const viewport = page.getViewport({ scale: backing });
    const context = canvas.getContext("2d");
    if (!context) throw new Error("this browser would not give the PDF a canvas to draw on");
    canvas.width = Math.floor(viewport.width);
    canvas.height = Math.floor(viewport.height);
    const css = { width: unit.width * cssScale, height: unit.height * cssScale };
    canvas.style.width = `${css.width}px`;
    canvas.style.height = `${css.height}px`;
    const render = page.render({ canvas, canvasContext: context, viewport });
    task = render;
    await render.promise;
    return css;
  })();
  return {
    done,
    cancel() {
      cancelled = true;
      task?.cancel();
    },
  };
}
