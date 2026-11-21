// @vitest-environment happy-dom
import { flushPromises } from "@vue/test-utils";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { nextTick } from "vue";

import QuickLook from "./QuickLook.vue";
import { harness, mountInShell, type Harness } from "../testing/shell-harness.ts";

/**
 * Each viewer Quick Look owes, as the operator meets it.
 *
 * Mounted on the real shell (`shell-harness.ts`): the bus, the window store
 * and the app table are the production ones, and only the edges are faked --
 * the socket, and `fetch` standing in for the content route. pdf.js is
 * replaced by a stand-in document, because what is under test here is the
 * paging, not pdf.js (which `shell/pdf.test.ts` opens for real).
 */

const pdf = vi.hoisted(() => ({ rendered: [] as number[], pages: 40, fail: null as string | null }));

vi.mock("../shell/pdf.ts", () => ({
  openPdf: async () => {
    if (pdf.fail) throw new Error(pdf.fail);
    return {
      numPages: pdf.pages,
      getPage: async () => ({}),
      destroy: async () => {},
    };
  },
  renderPage: (_doc: unknown, n: number) => {
    pdf.rendered.push(n);
    return { done: Promise.resolve({ width: 100, height: 100 }), cancel() {} };
  },
}));

interface Served {
  body: Uint8Array | string;
  status?: number;
  /** The file's full length, for a ranged answer. */
  total?: number;
}

/** What the fake content route was asked for. */
let asked: Array<{ path: string; range: string | null }> = [];

function serve(files: Record<string, Served>) {
  vi.stubGlobal(
    "fetch",
    vi.fn(async (url: string, init?: { headers?: Record<string, string> }) => {
      const path = new URLSearchParams(url.split("?")[1] ?? "").get("path") ?? "";
      const range = init?.headers?.Range ?? null;
      asked.push({ path, range });
      const f = files[path];
      if (!f) return new Response("there is no file at that address", { status: 404 });
      const bytes = typeof f.body === "string" ? new TextEncoder().encode(f.body) : f.body;
      const headers: Record<string, string> = {};
      if (f.total !== undefined) headers["Content-Range"] = `bytes 0-${bytes.length - 1}/${f.total}`;
      return new Response(bytes, { status: f.status ?? (f.total !== undefined ? 206 : 200), headers });
    }),
  );
}

beforeEach(() => {
  asked = [];
  pdf.rendered = [];
  pdf.pages = 40;
  pdf.fail = null;
});
afterEach(() => vi.unstubAllGlobals());

function invokes(rig: Harness, command: string) {
  return rig
    .socket()
    .frames()
    .filter((f) => f.type === "invoke" && f.command === command);
}

async function answer(rig: Harness, command: string, value: unknown) {
  await flushPromises();
  const call = invokes(rig, command).at(-1);
  expect(call, `the shell never asked for ${command}`).toBeTruthy();
  rig.socket().deliver({ type: "response", id: call!.id, outcome: { status: "ok", value } });
  await flushPromises();
}

async function openOn(path: string, options: { siblings?: string[]; connect?: boolean } = {}) {
  const rig = harness();
  if (options.connect) rig.connect();
  const { wrapper } = mountInShell(QuickLook, { harness: rig, attachTo: document.body });
  rig.shell.quicklook.open(path, options.siblings);
  await flushPromises();
  return { rig, wrapper };
}

function pointer(el: Element, type: string, x: number, y = 100, id = 1) {
  el.dispatchEvent(
    new PointerEvent(type, {
      pointerId: id,
      pointerType: "touch",
      isPrimary: id === 1,
      clientX: x,
      clientY: y,
      bubbles: true,
    }),
  );
}

describe("text and source", () => {
  it("is fetched as a bounded head of the file and shown as text", async () => {
    serve({ "/src/main.rs": { body: "fn main() {\n    println!(\"<b>hi</b>\");\n}\n" } });
    const { wrapper } = await openOn("/src/main.rs");

    expect(asked[0]?.range, "the whole file was asked for").toMatch(/^bytes=0-\d+$/);
    const pre = wrapper.find("pre");
    expect(pre.text()).toContain('println!("<b>hi</b>");');
    expect(wrapper.find("pre b").exists(), "source was rendered as markup").toBe(false);
    wrapper.unmount();
  });

  it("says when only the start of a large file is shown", async () => {
    serve({ "/server.log": { body: "line\n".repeat(100), total: 300_000_000 } });
    const { wrapper } = await openOn("/server.log");
    expect(wrapper.text()).toMatch(/first .* of 300 MB/);
    wrapper.unmount();
  });

  it("refuses a binary file rather than showing noise", async () => {
    serve({ "/a.dat": { body: new Uint8Array([0x7f, 0x45, 0x4c, 0x46, 0, 0, 1, 2]) } });
    const { wrapper } = await openOn("/a.dat");
    expect(wrapper.find("pre").exists()).toBe(false);
    expect(wrapper.text()).toMatch(/binary/i);
    wrapper.unmount();
  });

  it("says what the node said when the file cannot be read", async () => {
    serve({});
    const { wrapper } = await openOn("/gone.txt");
    expect(wrapper.text()).toContain("there is no file at that address");
    wrapper.unmount();
  });

  it("shows an empty file as empty text, though the route refuses a range on it", async () => {
    // A zero-length file has no byte 0, so `Range: bytes=0-...` is answered
    // 416; that is still an empty text file, not a file that cannot be read.
    serve({ "/pkg/__init__.py": { body: "", status: 416 } });
    const { wrapper } = await openOn("/pkg/__init__.py");
    expect(wrapper.find("pre").exists(), "an empty file was not shown as text").toBe(true);
    expect(wrapper.text()).not.toMatch(/could not load/i);
    wrapper.unmount();
  });

  it("puts the line count in the details panel", async () => {
    serve({ "/notes.txt": { body: "one\ntwo\nthree\n" } });
    const { rig, wrapper } = await openOn("/notes.txt", { connect: true });
    await wrapper.find("button[aria-label='File details']").trigger("click");
    await answer(rig, "files.stat", { size: 14, modified: null });
    const info = wrapper.find("aside dl").text();
    expect(info).toMatch(/Lines\s*3/);
    wrapper.unmount();
  });
});

describe("Markdown", () => {
  it("is rendered as a document, with nothing in it able to run", async () => {
    serve({
      "/docs/README.md": {
        body: "# Hello\n\n<script>window.pwned = 1</script>\n\n<img src=x onerror=alert(1)>\n\n![shot](img/a.png)",
      },
    });
    const { wrapper } = await openOn("/docs/README.md");

    expect(wrapper.find("article h1").text()).toBe("Hello");
    expect(wrapper.find("article script").exists()).toBe(false);
    expect(wrapper.find("article img[onerror]").exists()).toBe(false);
    // The file's own screenshot, fetched from beside it through the route.
    expect(wrapper.find("article img").attributes("src")).toBe(
      "/content?path=%2Fdocs%2Fimg%2Fa.png",
    );
    wrapper.unmount();
  });

  it("can be read as its source", async () => {
    serve({ "/a.md": { body: "# Title" } });
    const { wrapper } = await openOn("/a.md");
    await wrapper.findAll("button").find((b) => b.text() === "Source")!.trigger("click");
    expect(wrapper.find("pre").text()).toBe("# Title");
    wrapper.unmount();
  });
});

describe("a web page", () => {
  it("can be shown as its source, as text", async () => {
    serve({ "/site/index.html": { body: "<h1>Hi</h1><script>window.pwned=1</script>" } });
    const { wrapper } = await openOn("/site/index.html");
    expect(wrapper.find("iframe[sandbox]").exists()).toBe(true);

    const toggle = wrapper.findAll("button").find((b) => b.text() === "Source")!;
    await toggle.trigger("click");
    await flushPromises();

    expect(wrapper.find("iframe").exists()).toBe(false);
    expect(wrapper.find("pre").text()).toContain("<script>window.pwned=1</script>");
    expect(document.querySelector(".scrim script")).toBeNull();
    wrapper.unmount();
  });

  it("goes back to the rendered page on the next file, rather than keeping the source view", async () => {
    serve({ "/site/a.html": { body: "<p>a</p>" }, "/site/b.html": { body: "<p>b</p>" } });
    const { rig, wrapper } = await openOn("/site/a.html", { siblings: ["/site/a.html", "/site/b.html"] });
    await wrapper.findAll("button").find((b) => b.text() === "Source")!.trigger("click");
    await flushPromises();
    expect(wrapper.find("iframe").exists()).toBe(false);

    await wrapper.find("[role=dialog]").trigger("keydown", { key: "ArrowRight" });
    await flushPromises();
    expect(rig.shell.quicklook.current.value).toBe("/site/b.html");
    expect(wrapper.find("iframe[sandbox]").exists(), "the source view carried over to the next file").toBe(true);
    wrapper.unmount();
  });
});

describe("an archive", () => {
  it("is listed by the node, without anything being extracted", async () => {
    const { rig, wrapper } = await openOn("/pkg.zip", { connect: true });

    expect(invokes(rig, "files.archive.list").at(-1)?.params).toEqual({ path: "/pkg.zip" });
    await answer(rig, "files.archive.list", {
      path: "r620:/pkg.zip",
      format: "zip",
      entries: [
        { name: "pkg/", is_dir: true, is_link: false, size: 0, modified: null, encrypted: false },
        { name: "pkg/readme.txt", is_dir: false, is_link: false, size: 1500, modified: 1_710_510_330, encrypted: false },
      ],
      total_entries: 2,
      truncated: false,
      stopped: null,
    });

    const rows = wrapper.findAll("tbody tr");
    expect(rows.map((r) => r.find(".name").text())).toEqual([
      expect.stringContaining("pkg/"),
      expect.stringContaining("pkg/readme.txt"),
    ]);
    expect(rows[1]!.text()).toContain("1.5 KB");
    wrapper.unmount();
  });

  it("says when the listing is only part of the archive, and why", async () => {
    const { rig, wrapper } = await openOn("/big.tar.gz", { connect: true });
    await answer(rig, "files.archive.list", {
      format: "tar_gzip",
      entries: [{ name: "a", is_dir: false, is_link: false, size: 1, modified: null, encrypted: false }],
      total_entries: null,
      truncated: true,
      stopped: "stopped after inflating 512 MiB",
    });
    expect(wrapper.text()).toMatch(/showing the first 1/i);
    expect(wrapper.text()).toContain("stopped after inflating 512 MiB");
    wrapper.unmount();
  });

  it("shows the node's reason when it cannot list one", async () => {
    const { rig, wrapper } = await openOn("/bundle.zip", { connect: true });
    await flushPromises();
    const call = invokes(rig, "files.archive.list").at(-1)!;
    rig.socket().deliver({
      type: "response",
      id: call.id,
      outcome: { status: "error", error: { kind: "failed", message: "this zip has no end-of-directory record" } },
    });
    await flushPromises();
    expect(wrapper.text()).toContain("end-of-directory");
    wrapper.unmount();
  });
});

describe("a PDF", () => {
  it("pages through, by button and by key, and stops at the ends", async () => {
    const { wrapper } = await openOn("/report.pdf");
    expect(wrapper.find("[data-pages]").text()).toContain("40");
    expect(pdf.rendered.at(-1)).toBe(1);

    await wrapper.find("button[aria-label='Next page']").trigger("click");
    expect(pdf.rendered.at(-1)).toBe(2);

    await wrapper.find("[role=dialog]").trigger("keydown", { key: "PageDown" });
    expect(pdf.rendered.at(-1)).toBe(3);

    await wrapper.find("[role=dialog]").trigger("keydown", { key: "End" });
    expect(pdf.rendered.at(-1)).toBe(40);
    await nextTick();
    expect(wrapper.find("button[aria-label='Next page']").attributes()).toHaveProperty("disabled");
    await wrapper.find("[role=dialog]").trigger("keydown", { key: "PageDown" });
    expect(pdf.rendered.at(-1), "paged past the last page").toBe(40);

    await wrapper.find("[role=dialog]").trigger("keydown", { key: "Home" });
    expect(pdf.rendered.at(-1)).toBe(1);
    wrapper.unmount();
  });

  it("says why when the document will not open", async () => {
    pdf.fail = "Invalid PDF structure.";
    const { wrapper } = await openOn("/broken.pdf");
    expect(wrapper.text()).toContain("Invalid PDF structure.");
    wrapper.unmount();
  });

  it("lets the page-number field keep its arrow keys instead of changing file", async () => {
    const { rig, wrapper } = await openOn("/a.pdf", { siblings: ["/z.pdf", "/a.pdf", "/b.pdf"] });
    const field = wrapper.find("input");
    expect(field.exists(), "no page-number field").toBe(true);
    await field.trigger("keydown", { key: "ArrowLeft" });
    await field.trigger("keydown", { key: "ArrowRight" });
    expect(rig.shell.quicklook.current.value, "a caret move in the field changed file").toBe("/a.pdf");

    await field.trigger("keydown", { key: "Escape" });
    expect(rig.shell.quicklook.visible.value, "Escape in the field did not close").toBe(false);
    wrapper.unmount();
  });
});

describe("a photograph", () => {
  it("zooms with the wheel, a double tap and the keyboard, and goes back to fitted", async () => {
    const { wrapper } = await openOn("/a.jpg");
    const box = wrapper.find("[data-scale]");
    expect(box.attributes("data-scale")).toBe("1.00");

    await box.trigger("wheel", { deltaY: -300 });
    expect(Number(box.attributes("data-scale"))).toBeGreaterThan(1);

    await wrapper.find("[role=dialog]").trigger("keydown", { key: "0" });
    expect(box.attributes("data-scale")).toBe("1.00");

    await box.trigger("dblclick");
    expect(box.attributes("data-scale")).toBe("2.50");
    await box.trigger("dblclick");
    expect(box.attributes("data-scale")).toBe("1.00");

    await wrapper.find("[role=dialog]").trigger("keydown", { key: "+" });
    expect(Number(box.attributes("data-scale"))).toBeGreaterThan(1);
    wrapper.unmount();
  });

  it("zooms with two fingers spreading", async () => {
    const { wrapper } = await openOn("/a.jpg");
    const box = wrapper.find("[data-scale]").element;
    pointer(box, "pointerdown", 150, 100, 1);
    pointer(box, "pointerdown", 250, 100, 2);
    pointer(box, "pointermove", 100, 100, 1);
    pointer(box, "pointermove", 300, 100, 2);
    pointer(box, "pointerup", 100, 100, 1);
    pointer(box, "pointerup", 300, 100, 2);
    await nextTick();
    expect(Number(box.getAttribute("data-scale"))).toBeCloseTo(2, 1);
    wrapper.unmount();
  });

  it("does not take a pinch for a swipe, though the first finger moved sideways", async () => {
    // The first finger lands while the photo is fitted, so it starts what
    // could be a swipe; the second turns it into a pinch. Lifting the first
    // finger after travelling 150px must not then change file.
    const { rig, wrapper } = await openOn("/a.jpg", { siblings: ["/a.jpg", "/b.jpg"] });
    const box = wrapper.find("[data-scale]").element;
    pointer(box, "pointerdown", 250, 100, 1);
    pointer(box, "pointerdown", 300, 100, 2);
    pointer(box, "pointermove", 100, 100, 1);
    pointer(box, "pointermove", 400, 100, 2);
    pointer(box, "pointerup", 100, 100, 1);
    pointer(box, "pointerup", 400, 100, 2);
    await nextTick();
    expect(rig.shell.quicklook.current.value).toBe("/a.jpg");
    wrapper.unmount();
  });

  it("moves through the folder on a swipe while fitted, and pans instead once zoomed", async () => {
    const siblings = ["/a.jpg", "/b.jpg", "/c.jpg"];
    const { rig, wrapper } = await openOn("/a.jpg", { siblings });

    const box = () => wrapper.find("[data-scale]").element;
    pointer(box(), "pointerdown", 300);
    pointer(box(), "pointerup", 150);
    await nextTick();
    expect(rig.shell.quicklook.current.value, "a left swipe did not move on").toBe("/b.jpg");

    pointer(box(), "pointerdown", 100);
    pointer(box(), "pointerup", 260);
    await nextTick();
    expect(rig.shell.quicklook.current.value, "a right swipe did not move back").toBe("/a.jpg");

    await wrapper.find("[data-scale]").trigger("dblclick");
    pointer(box(), "pointerdown", 300);
    pointer(box(), "pointerup", 150);
    await nextTick();
    expect(rig.shell.quicklook.current.value, "panning a zoomed photo changed file").toBe("/a.jpg");
    wrapper.unmount();
  });

  it("admits a picture the browser cannot decode", async () => {
    const { wrapper } = await openOn("/photo.heic.jpg");
    await wrapper.find("img").trigger("error");
    expect(wrapper.find("img").exists()).toBe(false);
    expect(wrapper.text()).toMatch(/could not decode/i);
    expect(wrapper.find("a[download]").exists()).toBe(true);
    wrapper.unmount();
  });

  it("tries the next file afresh after one the browser could not decode", async () => {
    const { rig, wrapper } = await openOn("/a.heic.jpg", { siblings: ["/a.heic.jpg", "/b.jpg"] });
    await wrapper.find("img").trigger("error");
    expect(wrapper.text()).toMatch(/could not decode/i);

    await wrapper.find("[role=dialog]").trigger("keydown", { key: "ArrowRight" });
    await flushPromises();
    expect(rig.shell.quicklook.current.value).toBe("/b.jpg");
    expect(wrapper.find("img").exists(), "the next photo was refused as well").toBe(true);
    expect(wrapper.text()).not.toMatch(/could not decode/i);
    wrapper.unmount();
  });
});

describe("a video", () => {
  it("does not take a sideways drag on the player for a swipe: that is scrubbing", async () => {
    const { rig, wrapper } = await openOn("/a.mp4", { siblings: ["/a.mp4", "/b.mp4"] });
    const video = wrapper.find("video").element;
    pointer(video, "pointerdown", 300);
    pointer(video, "pointerup", 120);
    await nextTick();
    expect(rig.shell.quicklook.current.value, "scrubbing the timeline changed file").toBe("/a.mp4");

    // Beside the player, on the stage, the same drag is a swipe.
    const stage = wrapper.find(".stage").element;
    pointer(stage, "pointerdown", 300);
    pointer(stage, "pointerup", 120);
    await nextTick();
    expect(rig.shell.quicklook.current.value).toBe("/b.mp4");
    wrapper.unmount();
  });
});

describe("the details panel", () => {
  it("opens and closes with the i key", async () => {
    const { wrapper } = await openOn("/clip.mp4", { connect: true });
    expect(wrapper.find("aside").exists()).toBe(false);
    await wrapper.find("[role=dialog]").trigger("keydown", { key: "i" });
    expect(wrapper.find("aside").exists(), "i did not open the details").toBe(true);
    await wrapper.find("[role=dialog]").trigger("keydown", { key: "i" });
    expect(wrapper.find("aside").exists(), "i did not close the details").toBe(false);
    wrapper.unmount();
  });

  it("shows size and time from the node, and what the player learned", async () => {
    const { rig, wrapper } = await openOn("/clip.mp4", { connect: true });
    const video = wrapper.find("video").element as HTMLVideoElement;
    Object.defineProperty(video, "duration", { value: 3723, configurable: true });
    Object.defineProperty(video, "videoWidth", { value: 1920, configurable: true });
    Object.defineProperty(video, "videoHeight", { value: 1080, configurable: true });
    video.dispatchEvent(new Event("loadedmetadata"));

    await wrapper.find("button[aria-label='File details']").trigger("click");
    await answer(rig, "files.stat", { size: 2_400_000_000, modified: 1_710_510_330 });

    const info = wrapper.find("[aria-label='File details'] dl, aside dl");
    expect(info.text()).toContain("2.4 GB");
    expect(info.text()).toContain("1:02:03");
    expect(info.text()).toContain("1920 × 1080");
    wrapper.unmount();
  });

  it("reads a photograph's EXIF from the head of the file", async () => {
    serve({ "/DSC_0001.jpg": { body: jpegWithCamera("NIKON Z 6"), total: 25_000_000 } });
    const { rig, wrapper } = await openOn("/DSC_0001.jpg", { connect: true });
    await wrapper.find("button[aria-label='File details']").trigger("click");
    await answer(rig, "files.stat", { size: 25_000_000, modified: null });
    await flushPromises();

    const exifAsk = asked.find((a) => a.path === "/DSC_0001.jpg");
    expect(exifAsk?.range, "the whole photograph was fetched for its EXIF").toMatch(/^bytes=0-\d+$/);
    expect(wrapper.find("aside dl").text()).toContain("NIKON Z 6");
    wrapper.unmount();
  });
});

describe("opening the file fully", () => {
  const editor = {
    name: "scribe",
    title: "Editor",
    version: "0.1.0",
    lifecycle: "on-demand",
    status: "live",
    compact: true,
    wide: true,
  };

  it("hands a text file to the installed editor and gets out of the way", async () => {
    serve({ "/notes.txt": { body: "hello" } });
    const rig = harness();
    rig.connect();
    await answer(rig, "system.apps", { apps: [editor] });
    const { wrapper } = mountInShell(QuickLook, { harness: rig, attachTo: document.body });
    rig.shell.quicklook.open("/notes.txt");
    await flushPromises();

    const button = wrapper.find("button[aria-label='Open in Editor']");
    expect(button.exists(), "no way to open the file fully").toBe(true);
    await button.trigger("click");
    await flushPromises();

    const opened = rig.shell.windows.all.value.find((w) => w.app === "scribe");
    expect(opened, "no editor window opened").toBeTruthy();
    // Once the window store carries a file (the frame bridge's lane adds
    // `launch`), the editor must have been opened on this one.
    if (opened && "launch" in opened) {
      expect((opened as { launch?: { path?: string } | null }).launch?.path).toBe("/notes.txt");
    }
    expect(rig.shell.quicklook.visible.value).toBe(false);
    wrapper.unmount();
  });

  it("gives the window store the file's path when the store can open an app on one", async () => {
    // The production wiring (`openAppVia`, shared with context.ts), checked
    // through the shell: a store with the open-app entry gets the path.
    serve({ "/src/main.rs": { body: "fn main() {}" } });
    const rig = harness();
    rig.connect();
    await answer(rig, "system.apps", { apps: [editor] });
    const asked: Array<{ app: string; path?: string }> = [];
    Object.assign(rig.shell.windows, {
      openApp: async (app: string, options: { path?: string }) => {
        asked.push({ app, path: options.path });
        return "";
      },
    });
    const { wrapper } = mountInShell(QuickLook, { harness: rig, attachTo: document.body });
    rig.shell.quicklook.open("/src/main.rs");
    await flushPromises();
    await wrapper.find("button[aria-label='Open in Editor']").trigger("click");
    await flushPromises();

    expect(asked).toEqual([{ app: "scribe", path: "/src/main.rs" }]);
    wrapper.unmount();
  });

  it("offers no such button when no editor is installed", async () => {
    serve({ "/notes.txt": { body: "hello" } });
    const { wrapper } = await openOn("/notes.txt", { connect: true });
    expect(wrapper.find("button[aria-label^='Open in']").exists()).toBe(false);
    wrapper.unmount();
  });
});

/** A minimal JPEG whose EXIF names a camera model (little-endian TIFF). */
function jpegWithCamera(model: string): Uint8Array {
  const text = new TextEncoder().encode(model + "\0");
  const tiff = new Uint8Array(8 + 2 + 12 + 4 + text.length);
  const v = new DataView(tiff.buffer);
  tiff.set([0x49, 0x49]);
  v.setUint16(2, 42, true);
  v.setUint32(4, 8, true);
  v.setUint16(8, 1, true);
  v.setUint16(10, 0x0110, true);
  v.setUint16(12, 2, true);
  v.setUint32(14, text.length, true);
  v.setUint32(18, 26, true);
  v.setUint32(22, 0, true);
  tiff.set(text, 26);
  const app1 = [...new TextEncoder().encode("Exif\0\0"), ...tiff];
  return new Uint8Array([
    0xff, 0xd8,
    0xff, 0xe1, (app1.length + 2) >> 8, (app1.length + 2) & 0xff, ...app1,
    0xff, 0xda, 0x00, 0x02,
  ]);
}
