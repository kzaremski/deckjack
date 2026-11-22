// @vitest-environment happy-dom
import { readFileSync } from "node:fs";
import { resolve } from "node:path";
import { afterEach, describe, expect, it } from "vitest";

import {
  AUTHOR_KEY,
  anchoredScroll,
  clampZoom,
  local,
  mountOffice,
  pinchZoom,
  renderWidth,
  selectionAnchor,
  swipeDirection,
  threads,
} from "../../../crates/deckjack-office/ui/office.js";

/**
 * The Office app's UI, mounted from its real markup with a fake bus. What it
 * promises: pages are drawn lazily, the reading view shows changes and
 * comments as such, a selection becomes a comment anchored by the same
 * UTF-16 offsets the node counts, and nothing ever replaces the original
 * except through the trash.
 */

const html = readFileSync(resolve(process.cwd(), "../crates/deckjack-office/ui/index.html"), "utf8");
const markup = html.slice(html.indexOf("<body>") + 6, html.indexOf("<script"));

// Shaped like `office.text`'s answer.
const READING = {
  blocks: [
    { type: "paragraph", anchor: "p/0", heading: 1, list: null, inlines: [{ type: "text", text: "Quarterly report" }] },
    {
      type: "paragraph",
      anchor: "p/1",
      heading: null,
      list: null,
      inlines: [
        { type: "text", text: "Revenue " },
        { type: "text", text: "grew 😀 fast", comments: ["0"] },
        { type: "object", what: "picture" },
        { type: "text", text: " and " },
        { type: "text", text: "might", change: 0 },
        { type: "text", text: "will", change: 1 },
        { type: "text", text: " hire." },
      ],
    },
    {
      type: "table",
      rows: [[{ span: 2, blocks: [{ type: "paragraph", anchor: "p/2", inlines: [{ type: "text", text: "Cell" }] }] }]],
    },
    { type: "paragraph", anchor: "p/3", list: { num_id: "2", level: 1, marker: "a)" }, inlines: [{ type: "text", text: "Sub" }] },
  ],
  comments: [
    { id: "0", author: "Ann Lee", date: "2026-11-01T09:00:00Z", text: "Source?", reply_to: null, done: false },
    { id: "1", author: "Bob Ray", date: "2026-11-02T09:00:00Z", text: "Slide 4.", reply_to: "0", done: false },
  ],
  anchored: ["0"],
  changes: [
    { id: "10", kind: "deletion", author: "Bob Ray", date: "2026-11-02T10:00:00Z", anchor: "p/1", text: "might" },
    { id: "11", kind: "insertion", author: "Ann Lee", date: "2026-11-03T11:00:00Z", anchor: "p/1", text: "will" },
  ],
};

const PNG = "iVBORw0KGgo=";

function memory(seed = {}) {
  const data = new Map(Object.entries(seed));
  return { read: (k) => (data.has(k) ? data.get(k) : null), write: (k, v) => data.set(k, String(v)), data };
}

function rig({ storage = memory(), engine = true, files = {} } = {}) {
  document.body.innerHTML = markup;
  const calls = [];
  const looked = [];
  const disk = new Set(["/Documents/Report.docx", ...Object.keys(files)]);
  let copies = 0;
  const timers = [];
  const bus = {
    async invoke(command, params, onProgress) {
      calls.push([command, params]);
      switch (command) {
        case "office.open":
          return {
            path: params.path,
            kind: /\.pptx$/.test(params.path) ? "presentation" : "document",
            reading: /\.docx$/.test(params.path),
            engine,
            comment: { ok: /\.docx$/.test(params.path), note: null },
            keeps: "Every other part is copied byte for byte.",
          };
        case "office.pages":
          onProgress?.({ message: "Laying out pages" });
          return {
            pages: 8,
            sizes: Array.from({ length: 8 }, (_, i) => (i === 2 ? { width: 792, height: 612 } : { width: 612, height: 792 })),
          };
        case "office.page.render":
          return { page: params.page, width: params.width, mime: "image/png", data: PNG };
        case "office.text":
          return READING;
        case "office.comment.add": {
          copies += 1;
          const path = copies === 1 ? "/Documents/Report (commented).docx" : `/Documents/Report (commented ${copies}).docx`;
          disk.add(path);
          return { path: `r620:${path}`, id: String(1 + copies) };
        }
        case "files.trash":
          for (const p of params.paths) disk.delete(p);
          return { trashed: params.paths.map((p, i) => ({ id: String(i), path: p })) };
        case "office.save.inplace":
          if (disk.has(params.path)) throw new Error(`${params.path} already exists`);
          disk.delete(params.prepared);
          disk.add(params.path);
          return { path: params.path };
        case "office.export_pdf":
          return { path: "r620:/Documents/Report.pdf" };
        case "office.cancel":
          return { cancelled: true };
        case "files.list":
          return { path: params.path, entries: [{ name: "Report.docx", path: `r620:${params.path}/Report.docx`, kind: "file" }] };
        default:
          throw new Error(`unexpected ${command}`);
      }
    },
  };
  const shell = { quicklook: (path, siblings) => looked.push([path, siblings]) };
  const office = mountOffice(document, {
    bus,
    shell,
    storage,
    win: window,
    IntersectionObserver: null,
    later: (ms, fn) => {
      timers.push(fn);
      return () => {};
    },
  });
  const named = (command) => calls.filter(([c]) => c === command).map(([, p]) => p);
  return { office, calls, named, looked, disk, timers, storage };
}

/** Select [start, end) of a paragraph's text, by DOM text offsets. */
function select(anchor, start, end) {
  const para = document.querySelector(`[data-anchor="${anchor}"]`);
  const nodes = [];
  const collect = (node) => {
    for (const child of node.childNodes) {
      if (child.nodeType === 3) nodes.push(child);
      else collect(child);
    }
  };
  collect(para);
  const locate = (offset) => {
    let left = offset;
    for (const node of nodes) {
      if (left <= node.data.length) return [node, left];
      left -= node.data.length;
    }
    return [nodes[nodes.length - 1], nodes[nodes.length - 1].data.length];
  };
  const range = document.createRange();
  range.setStart(...locate(start));
  range.setEnd(...locate(end));
  const sel = window.getSelection();
  sel.removeAllRanges();
  sel.addRange(range);
  document.dispatchEvent(new Event("selectionchange"));
  return range;
}

afterEach(() => {
  document.body.innerHTML = "";
});

describe("the arithmetic", () => {
  it("zooms with the fingers, inside its bounds", () => {
    expect(pinchZoom(1, 100, 200)).toBe(2);
    expect(pinchZoom(2, 100, 10)).toBe(0.5);
    expect(pinchZoom(3, 100, 400)).toBe(4);
    expect(pinchZoom(1.5, 0, 50)).toBe(1.5);
    expect(clampZoom(Number.NaN)).toBe(1);
  });

  it("keeps the point under the fingers still while zooming", () => {
    // 100 px scrolled, fingers 50 px into the view: that content point is at
    // 150 and moves to 300 at twice the size, so the scroll becomes 250.
    expect(anchoredScroll(100, 50, 1, 2)).toBe(250);
  });

  it("asks for the width the page occupies times the pixel ratio, in cache steps", () => {
    expect(renderWidth(360, 3)).toBe(1100);
    expect(renderWidth(800, 2, 2)).toBe(2400);
    expect(renderWidth(50, 1)).toBe(200);
  });

  it("turns the page only for a quick sideways swipe", () => {
    expect(swipeDirection(-120, 10, 200)).toBe(1);
    expect(swipeDirection(120, 10, 200)).toBe(-1);
    expect(swipeDirection(-120, 100, 200), "too diagonal").toBe(0);
    expect(swipeDirection(-30, 0, 200), "too short").toBe(0);
    expect(swipeDirection(-120, 0, 2000), "too slow").toBe(0);
  });

  it("threads replies under their root, roots in reading order", () => {
    const list = threads(
      [
        { id: "5", reply_to: null },
        { id: "0", reply_to: null },
        { id: "6", reply_to: "0" },
      ],
      ["0", "5"],
    );
    expect(list.map((t) => t.root.id)).toEqual(["0", "5"]);
    expect(list[0].replies.map((r) => r.id)).toEqual(["6"]);
  });

  it("strips the node from an address", () => {
    expect(local("r620:/Documents/a.docx")).toBe("/Documents/a.docx");
    expect(local("/Documents/a:b.docx")).toBe("/Documents/a:b.docx");
  });
});

describe("opening a document", () => {
  it("lays out pages, draws the first few, and reads a Word file", async () => {
    const { office, named } = rig();
    await office.start("r620:/Documents/Report.docx");
    expect(named("office.open")).toEqual([{ path: "/Documents/Report.docx" }]);
    expect(named("office.pages")[0].path).toBe("/Documents/Report.docx");
    expect(document.querySelectorAll(".page")).toHaveLength(8);
    expect(document.getElementById("page-count").textContent).toBe("8");
    expect(document.querySelectorAll(".page")[2].style.aspectRatio).toBe("792 / 612");
    // Without an observer the first pages are drawn straight away, and no
    // more than the lookahead: the page on screen and three after it.
    expect(named("office.page.render").map((p) => p.page)).toEqual([1, 2, 3, 4]);
    expect(document.querySelector(".page img").src).toMatch(/^blob:/);
    expect(document.getElementById("view-read").hidden).toBe(false);
  });

  it("shows tracked changes as changes, with who and when, and comments as threads", async () => {
    const { office, named } = rig();
    await office.start("/Documents/Report.docx");
    const del = document.querySelector("del");
    const ins = document.querySelector("ins");
    expect(del.textContent).toBe("might");
    expect(del.title).toMatch(/^Deleted by Bob Ray, /);
    expect(ins.title).toMatch(/^Inserted by Ann Lee, /);
    expect(document.querySelector("h1.para").textContent).toBe("Quarterly report");
    expect(document.querySelector('[data-anchor="p/3"]').dataset.marker).toBe("a)");
    expect(document.querySelector("td").colSpan).toBe(2);
    const thread = document.querySelector(".thread");
    expect(thread.textContent).toContain("Source?");
    expect(thread.querySelector(".reply").textContent).toContain("Slide 4.");
    expect(document.getElementById("comments-toggle").textContent).toBe("Comments (2)");
    // Only the structure: the agent text would double the reply, and a long
    // document only fits the core's frame limit as one or the other.
    expect(named("office.text")).toEqual([{ path: "/Documents/Report.docx", only: "blocks" }]);
  });

  it("keeps the placeholders and markers out of the text a selection counts", async () => {
    const { office } = rig();
    await office.start("/Documents/Report.docx");
    // The picture placeholder and list marker are generated content.
    expect(document.querySelector('[data-anchor="p/1"]').textContent).toBe("Revenue grew 😀 fast and mightwill hire.");
    expect(document.querySelector('[data-anchor="p/3"]').textContent).toBe("Sub");
  });

  it("offers no reading view or comments for a presentation", async () => {
    const { office, named } = rig();
    await office.start("/Documents/Deck.pptx");
    expect(document.getElementById("view-read").hidden).toBe(true);
    expect(document.getElementById("comments-toggle").hidden).toBe(true);
    expect(named("office.text")).toEqual([]);
  });

  it("says so when the node has no LibreOffice, and still reads a Word file", async () => {
    const { office, named } = rig({ engine: false });
    await office.start("/Documents/Report.docx");
    expect(named("office.pages")).toEqual([]);
    expect(document.getElementById("column").textContent).toMatch(/no LibreOffice/);
    expect(document.querySelector("del")).not.toBeNull();
  });

  it("opens from the picker, starting at the granted folders", async () => {
    const { office, named } = rig();
    await office.start(null);
    const rows = [...document.querySelectorAll("#picker-list .dj-row")].map((r) => r.dataset.path);
    expect(rows).toEqual(["/Documents", "/Downloads"]);
    document.querySelector('#picker-list [data-path="/Documents"]').click();
    await new Promise((r) => setTimeout(r, 0));
    expect(named("files.list")).toEqual([{ path: "/Documents" }]);
    document.querySelector('#picker-list [data-path="/Documents/Report.docx"]').click();
    await new Promise((r) => setTimeout(r, 0));
    expect(named("office.open")).toEqual([{ path: "/Documents/Report.docx" }]);
  });
});

describe("commenting", () => {
  it("anchors a selection by UTF-16 offsets into its paragraph", async () => {
    const { office } = rig();
    await office.start("/Documents/Report.docx");
    // "grew 😀 fast" starts at 8; the emoji is two units, so it ends at 20.
    const range = select("p/1", 8, 20);
    expect(selectionAnchor(document, range)).toMatchObject({ anchor: "p/1", start: 8, end: 20, text: "grew 😀 fast" });
  });

  it("cuts a selection that runs past its paragraph at the paragraph's end", async () => {
    const { office } = rig();
    await office.start("/Documents/Report.docx");
    const para = document.querySelector('[data-anchor="p/0"]');
    const next = document.querySelector('[data-anchor="p/1"]');
    const range = document.createRange();
    range.setStart(para.firstChild.firstChild, 10);
    range.setEnd(next.firstChild.firstChild, 3);
    expect(selectionAnchor(document, range)).toMatchObject({ anchor: "p/0", start: 10, end: 16, clipped: true });
  });

  it("writes the first comment into a copy and switches to it", async () => {
    const storage = memory({ [AUTHOR_KEY]: "Kim" });
    const { office, named, disk } = rig({ storage });
    await office.start("/Documents/Report.docx");
    document.getElementById("view-read").click();
    select("p/1", 0, 7);
    expect(document.getElementById("comment-button").hidden).toBe(false);
    document.getElementById("comment-button").click();
    expect(document.getElementById("composer").hidden).toBe(false);
    expect(document.getElementById("composer-author").value).toBe("Kim");
    document.getElementById("composer-text").value = "Which revenue?";
    document.getElementById("composer-save").click();
    await new Promise((r) => setTimeout(r, 0));
    await new Promise((r) => setTimeout(r, 0));
    expect(named("office.comment.add")).toEqual([
      { path: "/Documents/Report.docx", text: "Which revenue?", author: "Kim", anchor: "p/1", start: 0, end: 7 },
    ]);
    // Nothing went to the trash for a first comment: the original stays.
    expect(named("files.trash")).toEqual([]);
    expect(disk.has("/Documents/Report.docx")).toBe(true);
    expect(office.state.path).toBe("/Documents/Report (commented).docx");
    expect(document.getElementById("save-over").hidden).toBe(false);
    // Still saying so once the copy has loaded.
    expect(document.getElementById("status").textContent).toMatch(/^Comment saved in a copy: Report \(commented\)\.docx/);
  });

  it("puts later comments in the same copy, the previous version in the trash", async () => {
    const { office, named } = rig({ storage: memory({ [AUTHOR_KEY]: "Kim" }) });
    await office.start("/Documents/Report.docx");
    document.getElementById("view-read").click();
    for (const words of ["one", "two"]) {
      select("p/1", 0, 7);
      document.getElementById("comment-button").click();
      document.getElementById("composer-text").value = words;
      document.getElementById("composer-save").click();
      for (let i = 0; i < 4; i++) await new Promise((r) => setTimeout(r, 0));
    }
    expect(named("office.comment.add")[1].path).toBe("/Documents/Report (commented).docx");
    expect(named("files.trash")).toEqual([{ paths: ["/Documents/Report (commented).docx"] }]);
    expect(named("office.save.inplace")).toEqual([
      { prepared: "/Documents/Report (commented 2).docx", path: "/Documents/Report (commented).docx" },
    ]);
    expect(office.state.path).toBe("/Documents/Report (commented).docx");
    expect(document.getElementById("status").textContent).toMatch(/previous version is in the trash/);
  });

  it("replies to a thread", async () => {
    const { office, named } = rig({ storage: memory({ [AUTHOR_KEY]: "Kim" }) });
    await office.start("/Documents/Report.docx");
    document.querySelector(".reply-button").click();
    document.getElementById("composer-text").value = "Agreed";
    document.getElementById("composer-save").click();
    await new Promise((r) => setTimeout(r, 0));
    expect(named("office.comment.add")).toEqual([
      { path: "/Documents/Report.docx", text: "Agreed", author: "Kim", reply_to: "0" },
    ]);
  });

  it("asks for a name before saving a comment", async () => {
    const { office, named } = rig();
    await office.start("/Documents/Report.docx");
    document.getElementById("view-read").click();
    select("p/1", 0, 7);
    document.getElementById("comment-button").click();
    document.getElementById("composer-text").value = "x";
    document.getElementById("composer-save").click();
    await new Promise((r) => setTimeout(r, 0));
    expect(named("office.comment.add")).toEqual([]);
    expect(document.getElementById("status").textContent).toMatch(/name/);
  });

  it("saves over the original only on a second tap, through the trash", async () => {
    const { office, named, disk } = rig({ storage: memory({ [AUTHOR_KEY]: "Kim" }) });
    await office.start("/Documents/Report.docx");
    document.getElementById("view-read").click();
    select("p/1", 0, 7);
    document.getElementById("comment-button").click();
    document.getElementById("composer-text").value = "one";
    document.getElementById("composer-save").click();
    for (let i = 0; i < 4; i++) await new Promise((r) => setTimeout(r, 0));

    const button = document.getElementById("save-over");
    button.click();
    await new Promise((r) => setTimeout(r, 0));
    expect(named("files.trash"), "one tap trashed the original").toEqual([]);
    button.click();
    for (let i = 0; i < 4; i++) await new Promise((r) => setTimeout(r, 0));
    expect(named("files.trash")).toEqual([{ paths: ["/Documents/Report.docx"] }]);
    expect(named("office.save.inplace")).toEqual([
      { prepared: "/Documents/Report (commented).docx", path: "/Documents/Report.docx" },
    ]);
    expect(disk.has("/Documents/Report (commented).docx")).toBe(false);
    expect(office.state.path).toBe("/Documents/Report.docx");
    expect(button.hidden).toBe(true);
    expect(document.getElementById("status").textContent).toMatch(/^Saved over Report\.docx/);
  });
});

describe("export", () => {
  it("exports a PDF and previews it in Quick Look", async () => {
    const { office, named, looked } = rig();
    await office.start("/Documents/Report.docx");
    document.getElementById("export").click();
    await new Promise((r) => setTimeout(r, 0));
    expect(named("office.export_pdf")[0].path).toBe("/Documents/Report.docx");
    expect(looked).toEqual([["r620:/Documents/Report.pdf", ["r620:/Documents/Report.pdf"]]]);
  });
});
