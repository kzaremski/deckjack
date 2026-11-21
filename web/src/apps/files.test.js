// @vitest-environment happy-dom
import { readFileSync } from "node:fs";
import { resolve } from "node:path";
import { afterEach, beforeEach, describe, expect, it } from "vitest";

import { createSelection } from "../../public/kit/selection.js";
import { mountFiles } from "../../../crates/deckjack-files/ui/files.js";

/**
 * The Files app's UI, mounted from its real markup with a fake bus and a fake
 * shell. What it promises: selecting works by mouse, keyboard and touch, and
 * "open" on a file is Quick Look with the folder to step through.
 */

// Relative to `web/`, where vitest runs; `import.meta.url` is not a file URL
// under happy-dom.
const html = readFileSync(resolve(process.cwd(), "../crates/deckjack-files/ui/index.html"), "utf8");
const markup = html.slice(html.indexOf("<body>") + 6, html.indexOf("<script"));

// Shaped like the core's `FileInfo`: `size`, `modified` in Unix seconds.
const FOLDER = {
  "/": [
    { name: "b.txt", path: "/b.txt", kind: "file", size: 10, modified: 300 },
    { name: "Photos", path: "/Photos", kind: "dir", size: 0, modified: 100 },
    { name: "a.jpg", path: "/a.jpg", kind: "file", size: 2048, modified: 200 },
    { name: "out", path: "/out", kind: "symlink", size: 0, modified: null, escapes: true },
  ],
  "/Photos": [{ name: "c.png", path: "/Photos/c.png", kind: "file", size: 5, modified: 50 }],
};

const GiB = 1024 ** 3;

/** The core qualifies every address it returns with the node. */
const qualify = (p) => `r620:${p}`;

/** A storage double, because the page's own may be blocked. */
function memoryStorage(seed = {}) {
  const data = new Map(Object.entries(seed));
  return {
    getItem: (k) => (data.has(k) ? data.get(k) : null),
    setItem: (k, v) => data.set(k, String(v)),
    data,
  };
}

function rig(options = {}) {
  document.body.innerHTML = markup;
  const calls = [];
  const peeks = [];
  const timers = [];
  /** The node's folders, changed by the commands the way the core would. */
  const disk = structuredClone(FOLDER);
  const trashed = new Map();
  const folderOf = (path) => path.slice(0, path.lastIndexOf("/")) || "/";
  const take = (path) => {
    const list = disk[folderOf(path)];
    const i = list.findIndex((e) => e.path === path);
    return list.splice(i, 1)[0];
  };
  const put = (entry, dir) => {
    const path = dir === "/" ? `/${entry.name}` : `${dir}/${entry.name}`;
    (disk[dir] ??= []).push({ ...entry, path });
    return path;
  };
  const failures = {};
  const exists = (dir, name) => (disk[dir] ?? []).some((e) => e.name === name);
  const thumbs = { inflight: 0, max: 0, gates: [] };
  const state = {
    space: options.space ?? { total: 100 * GiB, free: 60 * GiB, available: 50 * GiB },
  };
  const bus = {
    async invoke(command, params, onProgress) {
      calls.push({ command, params });
      if (failures[command]) throw new Error(failures[command]);
      switch (command) {
        case "files.list":
          return {
            path: qualify(params.path),
            entries: (disk[params.path] ?? []).map((e) => ({ ...e, path: qualify(e.path) })),
          };
        case "files.space":
          return state.space;
        case "files.thumbnail": {
          if (!/\.(jpe?g|png|gif|webp|bmp)$/i.test(params.path)) {
            throw new Error(`${params.path} is not an image Files makes thumbnails of`);
          }
          thumbs.inflight++;
          thumbs.max = Math.max(thumbs.max, thumbs.inflight);
          if (options.holdThumbnails) await new Promise((res) => thumbs.gates.push(res));
          thumbs.inflight--;
          if (failures.thumbnailOf === params.path) throw new Error("undecodable");
          return { path: qualify(params.path), mime: "image/png", content: btoa(params.path) };
        }
        case "files.trash": {
          const base = trashed.size;
          const out = params.paths.map((p, i) => {
            const id = `t${base + i}`;
            trashed.set(id, take(p));
            return { id, from: p };
          });
          return { trashed: out };
        }
        case "files.trash.list":
          return {
            entries: [...trashed].map(([id, e]) => ({
              id,
              from: e.path,
              deleted_at: "2026-11-16T10:00:00",
              kind: e.kind,
              len: e.size ?? 0,
            })),
          };
        case "files.trash.restore":
          for (const id of params.ids) {
            const e = trashed.get(id);
            trashed.delete(id);
            put(e, folderOf(e.path));
          }
          return { paths: [] };
        case "files.move":
        case "files.copy": {
          // The core refuses a taken name unless a copy asks to keep both.
          const names = params.paths.map((p) => {
            const name = p.slice(p.lastIndexOf("/") + 1);
            if (!exists(params.to, name)) return name;
            if (command === "files.copy" && params.keep_both) {
              const dot = name.lastIndexOf(".");
              return dot > 0 ? `${name.slice(0, dot)} copy${name.slice(dot)}` : `${name} copy`;
            }
            throw new Error(`${params.to}/${name} already exists; delete or rename it first`);
          });
          if (options.holdCopies) await new Promise((res) => (r.releaseCopy = res));
          onProgress?.({ fraction: 0.5 });
          r.progressSeen = document.getElementById("status").textContent;
          const landed = params.paths.map((p, i) => {
            const entry = command === "files.move" ? take(p) : { ...findEntry(p) };
            return put({ ...entry, name: names[i] }, params.to);
          });
          return { paths: landed.map(qualify) };
        }
        default:
          return {};
      }
    },
  };
  const findEntry = (path) => disk[folderOf(path)].find((e) => e.path === path);
  const shell = {
    quicklook(path, siblings) {
      peeks.push({ path, siblings });
      return true;
    },
  };
  const opened = [];
  if (options.desktop) {
    // What /kit/shell.js answers, per the shell's own rules: an app that is
    // not installed is refused, with a reason. "refuses" is a frame with no
    // shell to ask at all; "no-editor" a desktop without the text editor;
    // "stale" one that said it had an editor and then refused (uninstalled
    // since Files asked).
    const installed = options.desktop === "no-editor" ? [] : ["scribe"];
    window.deckjackShell = {
      openApp(name, opts) {
        opened.push({ name, ...opts });
        if (options.desktop === "refuses") return false;
        if (options.desktop === "stale" || !installed.includes(name)) {
          return Promise.resolve({ ok: false, reason: `There is no app called ${name} installed on this node.` });
        }
        return Promise.resolve({ ok: true });
      },
      canOpen(name) {
        return Promise.resolve(installed.includes(name));
      },
    };
  }
  const storage = options.storage ?? memoryStorage();
  // Where a pointer is, for drops: the test says which element is under it.
  const under = new Map();
  const app = mountFiles(document, {
    bus,
    shell,
    createSelection,
    text: (v) => String(v),
    failure: (host, e) => (host.textContent = String(e)),
    // Long-press timers fire when the test says so.
    delay: (_ms, fn) => {
      const t = { fn, dead: false };
      timers.push(t);
      return () => (t.dead = true);
    },
    storage,
    hitTest: (x, y) => under.get(`${x},${y}`)?.() ?? null,
    IntersectionObserver: options.IntersectionObserver ?? null,
  });
  const fire = () => timers.splice(0).forEach((t) => !t.dead && t.fn());
  return { app, calls, peeks, fire, failures, disk, trashed, thumbs, state, opened, storage, under };
}

const list = () => document.getElementById("list");
const row = (path) => list().querySelector(`.dj-row[data-path="${path}"]`);
const selectedPaths = () =>
  [...list().querySelectorAll('.dj-row[aria-selected="true"]')].map((r) => r.dataset.path);

function click(path, init = {}) {
  row(path).dispatchEvent(new MouseEvent("click", { bubbles: true, ...init }));
}
function key(k, init = {}) {
  list().dispatchEvent(new KeyboardEvent("keydown", { key: k, bubbles: true, ...init }));
}
function pointer(type, path, init = {}) {
  const ev = new MouseEvent(type, { bubbles: true, clientX: 0, clientY: 0, ...init });
  Object.defineProperty(ev, "pointerType", { value: init.pointerType ?? "touch" });
  (path ? row(path) : list()).dispatchEvent(ev);
}
function tap(path) {
  pointer("pointerdown", path);
  const ev = new MouseEvent("click", { bubbles: true });
  Object.defineProperty(ev, "pointerType", { value: "touch" });
  row(path).dispatchEvent(ev);
}

let r;
beforeEach(async () => {
  delete window.deckjackShell;
  r = rig();
  await r.app.ready;
});
afterEach(() => {
  delete window.deckjackShell;
});

/** Remount with options, replacing the default rig. */
async function remount(options) {
  delete window.deckjackShell;
  r = rig(options);
  await r.app.ready;
  await new Promise((res) => setTimeout(res, 0));
}

describe("the listing", () => {
  it("puts folders first, then files by name", () => {
    const order = [...list().querySelectorAll(".dj-row")].map((e) => e.dataset.path);
    expect(order).toEqual(["/Photos", "/a.jpg", "/b.txt", "/out"]);
  });
});

describe("selecting with a mouse", () => {
  it("click selects one, Ctrl adds, Shift ranges", () => {
    click("/a.jpg");
    expect(selectedPaths()).toEqual(["/a.jpg"]);
    click("/out", { ctrlKey: true });
    expect(selectedPaths()).toEqual(["/a.jpg", "/out"]);
    click("/Photos", { shiftKey: true });
    expect(selectedPaths()).toEqual(["/Photos", "/a.jpg", "/b.txt", "/out"]);
  });

  it("says how many are selected", () => {
    click("/a.jpg");
    click("/b.txt", { metaKey: true });
    expect(document.getElementById("selcount").textContent).toBe("2 selected");
    expect(document.getElementById("selbar").hidden).toBe(false);
  });

  it("double-click on a folder enters it and starts with nothing selected", async () => {
    click("/a.jpg");
    row("/Photos").dispatchEvent(new MouseEvent("dblclick", { bubbles: true }));
    await new Promise((res) => setTimeout(res, 0));
    expect(r.app.here).toBe("/Photos");
    expect(selectedPaths()).toEqual([]);
  });
});

describe("Quick Look", () => {
  it("Space previews the file under the cursor, with the folder's files to step through", () => {
    click("/b.txt");
    key(" ");
    expect(r.peeks).toEqual([{ path: "/b.txt", siblings: ["/a.jpg", "/b.txt"] }]);
  });

  it("is not offered for a folder or for a link that leaves the working directory", () => {
    click("/Photos");
    key(" ");
    click("/out");
    key(" ");
    expect(r.peeks).toEqual([]);
    expect(document.getElementById("peek").disabled).toBe(true);
  });

  it("has a button, because a phone has no Space bar", () => {
    click("/a.jpg");
    const peek = document.getElementById("peek");
    expect(peek.disabled).toBe(false);
    peek.click();
    expect(r.peeks.map((p) => p.path)).toEqual(["/a.jpg"]);
  });

  it("Enter on a file is the same as Space", () => {
    click("/a.jpg");
    key("Enter");
    expect(r.peeks.map((p) => p.path)).toEqual(["/a.jpg"]);
  });
});

describe("the keyboard", () => {
  it("arrows move, Shift extends, Ctrl+A selects all, Escape clears", () => {
    key("ArrowDown");
    key("ArrowDown");
    expect(selectedPaths()).toEqual(["/a.jpg"]);
    key("ArrowDown", { shiftKey: true });
    expect(selectedPaths()).toEqual(["/a.jpg", "/b.txt"]);
    key("a", { ctrlKey: true });
    expect(selectedPaths()).toHaveLength(4);
    key("Escape");
    expect(selectedPaths()).toEqual([]);
  });

  it("a plain 'a' is not select-all", () => {
    key("a");
    expect(selectedPaths()).toEqual([]);
  });
});

describe("on a phone", () => {
  it("a tap opens rather than selects", () => {
    tap("/a.jpg");
    expect(r.peeks.map((p) => p.path)).toEqual(["/a.jpg"]);
    expect(selectedPaths()).toEqual([]);
  });

  it("a long-press enters selection mode with that row selected", () => {
    pointer("pointerdown", "/a.jpg");
    r.fire();
    expect(list().dataset.mode).toBe("select");
    expect(selectedPaths()).toEqual(["/a.jpg"]);
  });

  it("the tap that ends a long-press does not undo it", () => {
    pointer("pointerdown", "/a.jpg");
    r.fire();
    const ev = new MouseEvent("click", { bubbles: true });
    Object.defineProperty(ev, "pointerType", { value: "touch" });
    row("/a.jpg").dispatchEvent(ev);
    expect(selectedPaths()).toEqual(["/a.jpg"]);
  });

  it("in selection mode a tap toggles instead of opening", () => {
    pointer("pointerdown", "/a.jpg");
    r.fire();
    row("/a.jpg").dispatchEvent(new MouseEvent("click", { bubbles: true }));
    tap("/b.txt");
    expect(selectedPaths()).toEqual(["/a.jpg", "/b.txt"]);
    expect(r.peeks).toEqual([]);
  });

  it("a press that moves is a scroll, not a long-press", () => {
    pointer("pointerdown", "/a.jpg");
    pointer("pointermove", "/a.jpg", { clientX: 0, clientY: 40 });
    r.fire();
    expect(list().dataset.mode).toBe("");
    expect(selectedPaths()).toEqual([]);
  });

  it("Done leaves selection mode and clears it", () => {
    pointer("pointerdown", "/a.jpg");
    r.fire();
    document.getElementById("done").click();
    expect(selectedPaths()).toEqual([]);
    expect(list().dataset.mode).toBe("");
  });

  it("entering a folder leaves selection mode, so the next tap opens again", async () => {
    document.getElementById("select").click();
    tap("/Photos");
    key("Enter");
    await new Promise((res) => setTimeout(res, 0));
    expect(r.app.here).toBe("/Photos");
    expect(list().dataset.mode).toBe("");
    tap("/Photos/c.png");
    expect(r.peeks.map((p) => p.path)).toEqual(["/Photos/c.png"]);
  });

  it("Select turns selection mode on without a long-press", () => {
    document.getElementById("select").click();
    tap("/a.jpg");
    tap("/b.txt");
    expect(selectedPaths()).toEqual(["/a.jpg", "/b.txt"]);
  });
});

describe("renaming", () => {
  function input() {
    return list().querySelector("input.rename");
  }

  it("edits the name in place and renames within the same folder", async () => {
    click("/a.jpg");
    key("F2");
    expect(input(), "no field appeared").toBeTruthy();
    // The stem is selected, not the extension.
    expect([input().selectionStart, input().selectionEnd]).toEqual([0, 1]);
    input().value = "sunset.jpg";
    input().dispatchEvent(new KeyboardEvent("keydown", { key: "Enter", bubbles: true }));
    await new Promise((res) => setTimeout(res, 0));
    expect(r.calls.filter((c) => c.command === "files.rename")).toEqual([
      { command: "files.rename", params: { from: "/a.jpg", to: "/sunset.jpg" } },
    ]);
  });

  it("Escape cancels without renaming", async () => {
    click("/a.jpg");
    key("F2");
    input().value = "other.jpg";
    input().dispatchEvent(new KeyboardEvent("keydown", { key: "Escape", bubbles: true }));
    await new Promise((res) => setTimeout(res, 0));
    expect(r.calls.some((c) => c.command === "files.rename")).toBe(false);
  });

  it("refuses a name with a slash rather than moving the file somewhere", async () => {
    click("/a.jpg");
    key("F2");
    input().value = "../x.jpg";
    input().dispatchEvent(new KeyboardEvent("keydown", { key: "Enter", bubbles: true }));
    await new Promise((res) => setTimeout(res, 0));
    expect(r.calls.some((c) => c.command === "files.rename")).toBe(false);
    expect(document.getElementById("status").textContent).toContain("cannot contain");
  });

  it("is only offered for exactly one selected item", () => {
    click("/a.jpg");
    click("/b.txt", { ctrlKey: true });
    expect(document.getElementById("rename").disabled).toBe(true);
  });
});

// ---------------------------------------------------------------------------
// Cut, copy, paste and the trash
// ---------------------------------------------------------------------------

const settle = () => new Promise((res) => setTimeout(res, 0));
const button = (id) => document.getElementById(id);
const statusText = () => button("status").textContent;
const sent = (command) => r.calls.filter((c) => c.command === command).map((c) => c.params);

async function enter(path) {
  row(path).dispatchEvent(new MouseEvent("dblclick", { bubbles: true }));
  await settle();
}

describe("moving with Cut and Paste", () => {
  it("moves the selection into the folder you paste in", async () => {
    click("/a.jpg");
    click("/b.txt", { ctrlKey: true });
    key("x", { ctrlKey: true });
    expect(button("paste").hidden).toBe(false);
    expect(button("paste").textContent).toBe("Move 2 here");

    await enter("/Photos");
    button("paste").click();
    await settle();

    expect(sent("files.move")).toEqual([{ paths: ["/a.jpg", "/b.txt"], to: "/Photos" }]);
    // What arrived is what is selected, so the operator sees where it went.
    expect(selectedPaths()).toEqual(["/Photos/a.jpg", "/Photos/b.txt"]);
    // A move cannot be pasted twice: its sources are gone.
    expect(button("paste").hidden).toBe(true);
    expect(statusText()).toBe("Moved 2 items.");
  });

  it("keeps what it picked up when the paste fails, and says why", async () => {
    click("/a.jpg");
    key("x", { ctrlKey: true });
    await enter("/Photos");
    r.failures["files.move"] = "/Photos/a.jpg already exists; delete or rename it first";
    button("paste").click();
    await settle();
    expect(statusText()).toContain("already exists");
    expect(button("paste").hidden, "a failed paste lost the clipboard").toBe(false);
  });

  it("a plain 'x' or 'v' does nothing", async () => {
    click("/a.jpg");
    key("x");
    expect(button("paste").hidden).toBe(true);
    key("v");
    await settle();
    expect(sent("files.move")).toEqual([]);
  });
});

describe("copying with Copy and Paste", () => {
  it("copies, shows progress, and can be pasted again", async () => {
    click("/a.jpg");
    button("copy").click();
    expect(button("paste").textContent).toBe("Copy 1 here");
    await enter("/Photos");
    key("v", { ctrlKey: true });
    await settle();

    expect(sent("files.copy")).toEqual([{ paths: ["/a.jpg"], to: "/Photos" }]);
    expect(r.progressSeen).toBe("Copying… 50%");
    expect(button("paste").hidden, "a copy's clipboard was dropped").toBe(false);
  });
});

describe("the trash", () => {
  it("Delete moves the selection to the trash and offers Undo", async () => {
    click("/a.jpg");
    click("/b.txt", { ctrlKey: true });
    key("Delete");
    await settle();

    expect(sent("files.trash")).toEqual([{ paths: ["/a.jpg", "/b.txt"] }]);
    expect(row("/a.jpg")).toBeNull();
    expect(statusText()).toBe("Moved 2 items to the trash.");
    expect(button("undo").hidden).toBe(false);

    button("undo").click();
    await settle();
    expect(sent("files.trash.restore")).toEqual([{ ids: ["t0", "t1"] }]);
    expect(row("/a.jpg"), "Undo did not put it back").toBeTruthy();
    expect(button("undo").hidden).toBe(true);
  });

  it("has a button too, because a phone has no Delete key", async () => {
    click("/a.jpg");
    button("trash").click();
    await settle();
    expect(sent("files.trash")).toEqual([{ paths: ["/a.jpg"] }]);
  });

  it("drops a trashed item from what Cut picked up", async () => {
    click("/a.jpg");
    click("/b.txt", { ctrlKey: true });
    key("x", { ctrlKey: true });
    click("/a.jpg");
    key("Delete");
    await settle();
    expect(button("paste").textContent).toBe("Move 1 here");
  });

  it("is a view of its own, where the one action is Restore", async () => {
    click("/a.jpg");
    key("Delete");
    await settle();
    button("trashview").click();
    await settle();

    expect(r.app.view).toBe("trash");
    const names = [...list().querySelectorAll(".dj-row-name")].map((e) => e.textContent);
    expect(names, "a trash row should say where it came from").toEqual(["/a.jpg"]);
    expect(statusText()).toContain("Empty the trash");

    click("t0");
    expect(button("restore").hidden).toBe(false);
    for (const id of ["cut", "copy", "trash", "rename", "peek", "mkdir"]) {
      expect(button(id).hidden, `${id} is offered in the trash`).toBe(true);
    }
    // Enter does not open a trashed folder or preview a trashed file.
    key("Enter");
    key(" ");
    expect(r.peeks).toEqual([]);

    button("restore").click();
    await settle();
    expect(sent("files.trash.restore")).toEqual([{ ids: ["t0"] }]);
    expect(list().querySelectorAll(".dj-row")).toHaveLength(0);
  });

  it("does not enter a trashed folder as if its id were a path", async () => {
    click("/Photos");
    key("Delete");
    await settle();
    button("trashview").click();
    await settle();
    const listed = sent("files.list").length;
    click("t0");
    key("Enter");
    row("t0").dispatchEvent(new MouseEvent("dblclick", { bubbles: true }));
    await settle();
    expect(sent("files.list").length, "the trash id was listed as a folder").toBe(listed);
    expect(r.app.view).toBe("trash");
  });

  it("does not offer Paste inside the trash", async () => {
    click("/a.jpg");
    key("c", { ctrlKey: true });
    button("trashview").click();
    await settle();
    expect(button("paste").hidden).toBe(true);
    key("v", { ctrlKey: true });
    await settle();
    expect(sent("files.copy")).toEqual([]);
  });
});

// ---------------------------------------------------------------------------
// What the core actually sends
// ---------------------------------------------------------------------------

const names = () => [...list().querySelectorAll(".dj-row")].map((e) => e.dataset.path);
const meta = (path) => row(path).querySelector(".dj-row-meta").textContent;

describe("the layout", () => {
  // happy-dom does no layout, so this is the one structural check here. The
  // defect it pins is real and was seen in Chromium: with the selection bar
  // above the list, the first click of a double-click showed it, pushed
  // every row down, and the second click opened a different folder.
  it("keeps the selection bar below the rows, so showing it cannot move them", () => {
    const after = list().compareDocumentPosition(button("selbar"));
    expect(after & Node.DOCUMENT_POSITION_FOLLOWING).toBeTruthy();
  });
});

describe("the core's own listing shape", () => {
  it("shows each file's size from the listing", () => {
    expect(meta("/a.jpg")).toBe("2.0 KB");
    expect(meta("/b.txt")).toBe("10 B");
  });

  it("goes up from a folder to its parent although every address is node-qualified", async () => {
    await enter("/Photos");
    expect(r.app.here).toBe("/Photos");
    key("Backspace");
    await settle();
    expect(sent("files.list").at(-1)).toEqual({ path: "/" });
    expect(r.app.here).toBe("/");
    expect(names()).toContain("/a.jpg");
  });
});

// ---------------------------------------------------------------------------
// Sort and filter
// ---------------------------------------------------------------------------

function sortBy(key) {
  const select = button("sort");
  select.value = key;
  select.dispatchEvent(new Event("change", { bubbles: true }));
}
function typeFilter(value) {
  const input = button("filter");
  input.value = value;
  input.dispatchEvent(new Event("input", { bubbles: true }));
}

describe("sorting", () => {
  it("by size puts the largest first and keeps folders on top", () => {
    sortBy("size");
    expect(names()).toEqual(["/Photos", "/a.jpg", "/b.txt", "/out"]);
    button("sortdir").click();
    expect(names()).toEqual(["/Photos", "/out", "/b.txt", "/a.jpg"]);
  });

  it("by modified puts the newest first, unknown times last, and shows the time", () => {
    sortBy("modified");
    expect(names()).toEqual(["/Photos", "/b.txt", "/a.jpg", "/out"]);
    button("sortdir").click();
    expect(names()).toEqual(["/Photos", "/a.jpg", "/b.txt", "/out"]);
    expect(meta("/a.jpg")).toMatch(/^19\d\d-\d\d-\d\d \d\d:\d\d$/);
  });

  it("by kind groups by extension", () => {
    sortBy("kind");
    expect(names()).toEqual(["/Photos", "/out", "/a.jpg", "/b.txt"]);
  });

  it("by name sorts numbers as numbers", async () => {
    r.disk["/"] = ["file10.txt", "file9.txt", "File1.txt"].map((name, i) => ({
      name,
      path: `/${name}`,
      kind: "file",
      size: i,
      modified: i,
    }));
    await r.app.refresh();
    expect(names()).toEqual(["/File1.txt", "/file9.txt", "/file10.txt"]);
  });

  it("is remembered for the next time Files opens", async () => {
    sortBy("size");
    button("sortdir").click();
    const storage = r.storage;
    await remount({ storage });
    expect(button("sort").value).toBe("size");
    expect(names()).toEqual(["/Photos", "/out", "/b.txt", "/a.jpg"]);
  });

  it("ignores a remembered sort it does not understand", async () => {
    const storage = memoryStorage({ "deckjack.files.sort": '{"key":"evil","dir":7}' });
    await remount({ storage });
    expect(button("sort").value).toBe("name");
    expect(names()).toEqual(["/Photos", "/a.jpg", "/b.txt", "/out"]);
  });

  it("keeps the selection across a re-sort", () => {
    click("/b.txt");
    sortBy("size");
    expect(selectedPaths()).toEqual(["/b.txt"]);
  });
});

describe("filtering", () => {
  it("shows only names that match, ignoring case, and says how many of how many", () => {
    typeFilter("JPG");
    expect(names()).toEqual(["/a.jpg"]);
    expect(statusText()).toBe("1 of 4 items");
  });

  it("select-all selects what is shown, not what is hidden", () => {
    typeFilter(".");
    key("a", { ctrlKey: true });
    expect(selectedPaths()).toEqual(["/a.jpg", "/b.txt"]);
    // The count is of the selection itself, which is what a Delete would
    // act on: hidden rows must not be in it.
    expect(button("selcount").textContent).toBe("2 selected");
  });

  it("says when nothing matches rather than showing an empty folder", () => {
    typeFilter("zzz");
    expect(list().textContent).toContain("Nothing here matches");
  });

  it("Escape in the filter clears it", () => {
    typeFilter("jpg");
    button("filter").dispatchEvent(new KeyboardEvent("keydown", { key: "Escape", bubbles: true }));
    expect(names()).toHaveLength(4);
  });

  it("is cleared by going to another folder", async () => {
    typeFilter("Pho");
    await enter("/Photos");
    expect(button("filter").value).toBe("");
    expect(names()).toEqual(["/Photos/c.png"]);
  });

  it("/ in the list goes to the filter", () => {
    key("/");
    expect(document.activeElement).toBe(button("filter"));
  });
});

// ---------------------------------------------------------------------------
// Thumbnails
// ---------------------------------------------------------------------------

const thumbOf = (path) => row(path)?.querySelector("img.thumb");

describe("thumbnails", () => {
  it("are asked for image files only, at the row's size, and shown in the row", async () => {
    await settle();
    expect(sent("files.thumbnail")).toEqual([{ path: "/a.jpg", size: 128 }]);
    expect(thumbOf("/a.jpg").getAttribute("src")).toBe(`data:image/png;base64,${btoa("/a.jpg")}`);
    expect(thumbOf("/b.txt")).toBeFalsy();
  });

  it("are not asked for again on a refresh when the file has not changed", async () => {
    await settle();
    await r.app.refresh();
    await settle();
    expect(sent("files.thumbnail")).toHaveLength(1);
    expect(thumbOf("/a.jpg").getAttribute("src")).toContain("base64");
  });

  it("are asked for again when the file changed", async () => {
    await settle();
    r.disk["/"].find((e) => e.name === "a.jpg").modified = 999;
    await r.app.refresh();
    await settle();
    expect(sent("files.thumbnail")).toHaveLength(2);
  });

  it("survive a change of selection without being reloaded", async () => {
    await settle();
    const img = thumbOf("/a.jpg");
    click("/a.jpg");
    click("/b.txt", { ctrlKey: true });
    expect(thumbOf("/a.jpg")).toBe(img);
  });

  it("leave the icon in place when there is none", async () => {
    r.failures.thumbnailOf = "/Photos/c.png";
    await enter("/Photos");
    await settle();
    expect(sent("files.thumbnail").map((p) => p.path)).toContain("/Photos/c.png");
    expect(thumbOf("/Photos/c.png").hasAttribute("src")).toBe(false);
  });

  it("are asked for as rows scroll into view, not before", async () => {
    // A stand-in for the browser's observer that only says what the test
    // says is visible. What it is handed to watch is the row, because an
    // image with no source yet has no size and would never be seen.
    const watched = new Set();
    let report = null;
    class FakeObserver {
      constructor(fn) {
        report = (el, isIntersecting = true) => fn([{ target: el, isIntersecting }]);
      }
      observe(el) {
        watched.add(el);
      }
      unobserve(el) {
        watched.delete(el);
      }
      disconnect() {
        watched.clear();
      }
    }
    await remount({ IntersectionObserver: FakeObserver });
    expect(sent("files.thumbnail")).toEqual([]);
    expect([...watched]).toEqual([row("/a.jpg")]);

    // Seen leaving the view is not seen.
    report(row("/a.jpg"), false);
    await settle();
    expect(sent("files.thumbnail")).toEqual([]);

    report(row("/a.jpg"));
    await settle();
    expect(sent("files.thumbnail")).toEqual([{ path: "/a.jpg", size: 128 }]);
    expect(thumbOf("/a.jpg").getAttribute("src")).toContain("base64");
  });

  it("are asked for a few at a time, not a folder at once", async () => {
    await remount({ holdThumbnails: true });
    r.disk["/"] = Array.from({ length: 10 }, (_, i) => ({
      name: `p${i}.jpg`,
      path: `/p${i}.jpg`,
      kind: "file",
      size: 1,
      modified: 1,
    }));
    await r.app.refresh();
    await settle();
    expect(r.thumbs.inflight).toBe(3);
    // Let them all through, a batch at a time.
    for (let i = 0; i < 20 && r.thumbs.gates.length; i++) {
      r.thumbs.gates.splice(0).forEach((open) => open());
      await settle();
      await settle();
    }
    expect(r.thumbs.max).toBe(3);
    const asked = new Set(
      sent("files.thumbnail")
        .map((p) => p.path)
        .filter((p) => /^\/p\d\.jpg$/.test(p)),
    );
    expect(asked.size).toBe(10);
    expect(thumbOf("/p9.jpg").getAttribute("src")).toContain("base64");
  });
});

// ---------------------------------------------------------------------------
// Free space
// ---------------------------------------------------------------------------

describe("free space", () => {
  it("is shown for the working directory", async () => {
    await settle();
    expect(button("space").textContent).toBe("50 GB free");
    expect(button("space").hasAttribute("data-low")).toBe(false);
  });

  it("is flagged when it runs low", async () => {
    await remount({ space: { total: 100 * GiB, free: GiB, available: 0.5 * GiB } });
    expect(button("space").textContent).toBe("Only 512 MB free");
    expect(button("space").hasAttribute("data-low")).toBe(true);
  });

  it("is flagged under 5% free even with plenty of bytes left", async () => {
    await remount({ space: { total: 4000 * GiB, free: 120 * GiB, available: 100 * GiB } });
    expect(button("space").textContent).toBe("Only 100 GB free");
    expect(button("space").hasAttribute("data-low")).toBe(true);
  });

  it("is read again after a paste", async () => {
    await settle();
    click("/a.jpg");
    key("c", { ctrlKey: true });
    await enter("/Photos");
    r.state.space = { total: 100 * GiB, free: 60 * GiB, available: 40 * GiB };
    key("v", { ctrlKey: true });
    await settle();
    await settle();
    expect(button("space").textContent).toBe("40 GB free");
  });

  it("stops a copy that cannot fit before it starts", async () => {
    await remount({ space: { total: 100 * GiB, free: GiB, available: GiB } });
    r.disk["/"].push({ name: "movie.mkv", path: "/movie.mkv", kind: "file", size: 2 * GiB, modified: 1 });
    await r.app.refresh();
    await settle();
    click("/movie.mkv");
    key("c", { ctrlKey: true });
    await enter("/Photos");
    key("v", { ctrlKey: true });
    await settle();
    expect(sent("files.copy")).toEqual([]);
    expect(statusText()).toContain("Not enough room");
  });

  it("does not stop a move, which needs no room", async () => {
    await remount({ space: { total: 100 * GiB, free: GiB, available: GiB } });
    r.disk["/"].push({ name: "movie.mkv", path: "/movie.mkv", kind: "file", size: 2 * GiB, modified: 1 });
    await r.app.refresh();
    await settle();
    click("/movie.mkv");
    key("x", { ctrlKey: true });
    await enter("/Photos");
    key("v", { ctrlKey: true });
    await settle();
    expect(sent("files.move")).toEqual([{ paths: ["/movie.mkv"], to: "/Photos" }]);
    expect(statusText()).not.toContain("Not enough room");
  });

  it("leaves the line empty when it cannot be read", async () => {
    await settle();
    r.failures["files.space"] = "statvfs failed";
    await r.app.refresh();
    await settle();
    expect(button("space").textContent).toBe("");
  });
});

// ---------------------------------------------------------------------------
// Pasting where a name is taken
// ---------------------------------------------------------------------------

describe("pasting into the folder it came from", () => {
  it("a copy is named 'name copy' instead of being refused", async () => {
    click("/a.jpg");
    key("c", { ctrlKey: true });
    key("v", { ctrlKey: true });
    await settle();
    expect(sent("files.copy")).toEqual([{ paths: ["/a.jpg"], to: "/", keep_both: true }]);
    expect(names()).toContain("/a copy.jpg");
    expect(selectedPaths()).toEqual(["/a copy.jpg"]);
  });

  it("a move there does nothing and says so", async () => {
    click("/a.jpg");
    key("x", { ctrlKey: true });
    key("v", { ctrlKey: true });
    await settle();
    expect(sent("files.move")).toEqual([]);
    expect(statusText()).toBe("It is already here.");
  });

  it("a copy into another folder that has the name offers Keep both", async () => {
    r.disk["/Photos"].push({ name: "a.jpg", path: "/Photos/a.jpg", kind: "file", size: 1, modified: 1 });
    click("/a.jpg");
    key("c", { ctrlKey: true });
    await enter("/Photos");
    key("v", { ctrlKey: true });
    await settle();
    expect(statusText()).toContain("already exists");
    expect(sent("files.copy")).toEqual([{ paths: ["/a.jpg"], to: "/Photos" }]);
    expect(button("keepboth").hidden).toBe(false);

    button("keepboth").click();
    await settle();
    expect(sent("files.copy").at(-1)).toEqual({ paths: ["/a.jpg"], to: "/Photos", keep_both: true });
    expect(names()).toContain("/Photos/a copy.jpg");
    expect(button("keepboth").hidden).toBe(true);
  });

  it("Keep both cannot be pressed twice while the copy runs", async () => {
    await remount({ holdCopies: true });
    r.disk["/Photos"].push({ name: "a.jpg", path: "/Photos/a.jpg", kind: "file", size: 1, modified: 1 });
    click("/a.jpg");
    key("c", { ctrlKey: true });
    await enter("/Photos");
    key("v", { ctrlKey: true });
    await settle();
    expect(button("keepboth").hidden).toBe(false);
    button("keepboth").click();
    await settle();
    expect(button("keepboth").hidden, "a second press would copy it twice").toBe(true);
    r.releaseCopy();
    await settle();
  });

  it("a refused move is not offered Keep both", async () => {
    r.disk["/Photos"].push({ name: "a.jpg", path: "/Photos/a.jpg", kind: "file", size: 1, modified: 1 });
    click("/a.jpg");
    key("x", { ctrlKey: true });
    await enter("/Photos");
    key("v", { ctrlKey: true });
    await settle();
    expect(statusText()).toContain("already exists");
    expect(button("keepboth").hidden).toBe(true);
  });
});

// ---------------------------------------------------------------------------
// Drag and drop
// ---------------------------------------------------------------------------

/** Say what is under the pointer at (x, y). */
function place(x, y, el) {
  r.under.set(`${x},${y}`, el);
}

describe("dragging with a mouse", () => {
  it("drops a file onto a folder to move it there", async () => {
    place(40, 40, () => row("/Photos"));
    pointer("pointerdown", "/a.jpg", { pointerType: "mouse", clientX: 0, clientY: 0 });
    pointer("pointermove", "/a.jpg", { pointerType: "mouse", clientX: 40, clientY: 40 });
    expect(row("/Photos").hasAttribute("data-drop"), "the folder is not shown as a target").toBe(true);
    expect(button("ghost").hidden).toBe(false);
    pointer("pointerup", "/a.jpg", { pointerType: "mouse", clientX: 40, clientY: 40 });
    await settle();
    expect(sent("files.move")).toEqual([{ paths: ["/a.jpg"], to: "/Photos" }]);
    expect(row("/a.jpg")).toBeNull();
    expect(statusText()).toBe("Moved 1 item to Photos.");
    expect(button("ghost").hidden).toBe(true);
  });

  it("drags the whole selection when the row is part of it", async () => {
    place(40, 40, () => row("/Photos"));
    click("/a.jpg");
    click("/b.txt", { ctrlKey: true });
    pointer("pointerdown", "/b.txt", { pointerType: "mouse" });
    pointer("pointermove", "/b.txt", { pointerType: "mouse", clientX: 40, clientY: 40 });
    pointer("pointerup", "/b.txt", { pointerType: "mouse", clientX: 40, clientY: 40 });
    await settle();
    expect(sent("files.move")).toEqual([{ paths: ["/a.jpg", "/b.txt"], to: "/Photos" }]);
  });

  it("onto a breadcrumb moves it up", async () => {
    await enter("/Photos");
    place(40, 40, () => document.querySelector('.crumb[data-path="/"]'));
    pointer("pointerdown", "/Photos/c.png", { pointerType: "mouse" });
    pointer("pointermove", "/Photos/c.png", { pointerType: "mouse", clientX: 40, clientY: 40 });
    pointer("pointerup", "/Photos/c.png", { pointerType: "mouse", clientX: 40, clientY: 40 });
    await settle();
    expect(sent("files.move")).toEqual([{ paths: ["/Photos/c.png"], to: "/" }]);
  });

  it("a drop on a file, on itself, or on nothing moves nothing", async () => {
    place(10, 10, () => row("/b.txt"));
    place(20, 20, () => row("/Photos"));
    place(30, 30, () => null);
    for (const [x, y] of [
      [10, 10],
      [30, 30],
    ]) {
      pointer("pointerdown", "/a.jpg", { pointerType: "mouse" });
      pointer("pointermove", "/a.jpg", { pointerType: "mouse", clientX: x, clientY: y });
      pointer("pointerup", "/a.jpg", { pointerType: "mouse", clientX: x, clientY: y });
      await settle();
    }
    // A folder dragged onto itself.
    pointer("pointerdown", "/Photos", { pointerType: "mouse" });
    pointer("pointermove", "/Photos", { pointerType: "mouse", clientX: 20, clientY: 20 });
    expect(row("/Photos").hasAttribute("data-drop")).toBe(false);
    pointer("pointerup", "/Photos", { pointerType: "mouse", clientX: 20, clientY: 20 });
    await settle();
    expect(sent("files.move")).toEqual([]);
  });

  it("a small wobble is still a click, not a drag", () => {
    pointer("pointerdown", "/a.jpg", { pointerType: "mouse" });
    pointer("pointermove", "/a.jpg", { pointerType: "mouse", clientX: 2, clientY: 1 });
    pointer("pointerup", "/a.jpg", { pointerType: "mouse", clientX: 2, clientY: 1 });
    click("/a.jpg");
    expect(button("ghost").hidden).toBe(true);
    expect(selectedPaths()).toEqual(["/a.jpg"]);
  });

  it("the click that ends a drag does not change the selection", () => {
    place(40, 40, () => null);
    click("/b.txt");
    pointer("pointerdown", "/a.jpg", { pointerType: "mouse" });
    pointer("pointermove", "/a.jpg", { pointerType: "mouse", clientX: 40, clientY: 40 });
    pointer("pointerup", "/a.jpg", { pointerType: "mouse", clientX: 40, clientY: 40 });
    click("/a.jpg");
    expect(selectedPaths()).toEqual(["/b.txt"]);
  });

  it("a drag that starts on a thumbnail is not taken over by the browser's own image drag", async () => {
    await settle();
    const img = row("/a.jpg").querySelector("img.thumb");
    expect(img?.getAttribute("src"), "the thumbnail did not load").toBeTruthy();
    place(40, 40, () => row("/Photos"));
    const at = (type, x, y) => {
      const ev = new MouseEvent(type, { bubbles: true, cancelable: true, clientX: x, clientY: y });
      Object.defineProperty(ev, "pointerType", { value: "mouse" });
      img.dispatchEvent(ev);
      return ev;
    };
    at("pointerdown", 0, 0);
    at("pointermove", 6, 6);
    // What the browser does next with a draggable image: dragstart, and if
    // that is not cancelled, pointercancel for the pointer Files is tracking.
    const native = new Event("dragstart", { bubbles: true, cancelable: true });
    img.dispatchEvent(native);
    expect(native.defaultPrevented, "the browser's image drag was allowed to start").toBe(true);
    // happy-dom has no `draggable` property; the attribute is what browsers read.
    expect(img.getAttribute("draggable")).toBe("false");
    at("pointermove", 40, 40);
    at("pointerup", 40, 40);
    await settle();
    expect(sent("files.move")).toEqual([{ paths: ["/a.jpg"], to: "/Photos" }]);
  });

  it("only the primary button drags: a right-button move drops nothing", async () => {
    place(40, 40, () => row("/Photos"));
    pointer("pointerdown", "/a.jpg", { pointerType: "mouse", button: 2 });
    pointer("pointermove", "/a.jpg", { pointerType: "mouse", button: 2, clientX: 40, clientY: 40 });
    expect(button("ghost").hidden).toBe(true);
    pointer("pointerup", "/a.jpg", { pointerType: "mouse", button: 2, clientX: 40, clientY: 40 });
    await settle();
    expect(sent("files.move")).toEqual([]);
  });

  it("a dropped file leaves what Cut picked up, so Paste does not point at where it was", async () => {
    place(40, 40, () => row("/Photos"));
    click("/a.jpg");
    key("x", { ctrlKey: true });
    expect(button("paste").hidden).toBe(false);
    pointer("pointerdown", "/a.jpg", { pointerType: "mouse" });
    pointer("pointermove", "/a.jpg", { pointerType: "mouse", clientX: 40, clientY: 40 });
    pointer("pointerup", "/a.jpg", { pointerType: "mouse", clientX: 40, clientY: 40 });
    await settle();
    expect(sent("files.move")).toEqual([{ paths: ["/a.jpg"], to: "/Photos" }]);
    expect(button("paste").hidden, "Paste still offers the file that was dropped").toBe(true);
  });
});

describe("dragging on a phone", () => {
  it("a long-press then a move is a drag, and a drop on a folder moves", async () => {
    place(0, 80, () => row("/Photos"));
    pointer("pointerdown", "/a.jpg");
    r.fire();
    pointer("pointermove", "/a.jpg", { clientX: 0, clientY: 80 });
    expect(row("/Photos").hasAttribute("data-drop")).toBe(true);
    pointer("pointerup", "/a.jpg", { clientX: 0, clientY: 80 });
    await settle();
    expect(sent("files.move")).toEqual([{ paths: ["/a.jpg"], to: "/Photos" }]);
  });

  it("holds the page still while dragging, and only then", () => {
    const touchmove = () => {
      const ev = new Event("touchmove", { bubbles: true, cancelable: true });
      row("/a.jpg").dispatchEvent(ev);
      return ev.defaultPrevented;
    };
    pointer("pointerdown", "/a.jpg");
    expect(touchmove(), "a plain swipe must still scroll").toBe(false);
    r.fire();
    expect(touchmove(), "the page scrolled under a drag").toBe(true);
  });

  it("a cancelled pointer mid-drag moves nothing", async () => {
    place(0, 80, () => row("/Photos"));
    pointer("pointerdown", "/a.jpg");
    r.fire();
    pointer("pointermove", "/a.jpg", { clientX: 0, clientY: 80 });
    expect(row("/Photos").hasAttribute("data-drop")).toBe(true);
    pointer("pointercancel", "/a.jpg", { clientX: 0, clientY: 80 });
    await settle();
    expect(sent("files.move"), "a gesture the system took away was treated as a drop").toEqual([]);
    expect(button("ghost").hidden).toBe(true);
  });

  it("the context menu a long-press also fires does not open over it", () => {
    pointer("pointerdown", "/a.jpg");
    r.fire();
    rightClick("/a.jpg");
    expect(button("menu").hidden).toBe(true);
    expect(selectedPaths()).toEqual(["/a.jpg"]);
  });

  it("a move before the hold is a scroll: no drag, no selection", async () => {
    place(0, 80, () => row("/Photos"));
    pointer("pointerdown", "/a.jpg");
    pointer("pointermove", "/a.jpg", { clientX: 0, clientY: 80 });
    r.fire();
    pointer("pointermove", "/a.jpg", { clientX: 0, clientY: 90 });
    pointer("pointerup", "/a.jpg", { clientX: 0, clientY: 80 });
    await settle();
    expect(sent("files.move")).toEqual([]);
    expect(selectedPaths()).toEqual([]);
  });

  it("scrolls the list when held near its edge, so a folder off screen can be reached", async () => {
    // A list 400px tall; the finger is held near its bottom edge.
    list().getBoundingClientRect = () => ({ top: 0, bottom: 400, left: 0, right: 300, width: 300, height: 400, x: 0, y: 0 });
    Object.defineProperty(list(), "scrollHeight", { value: 4000, configurable: true });
    Object.defineProperty(list(), "clientHeight", { value: 400, configurable: true });
    pointer("pointerdown", "/a.jpg", { clientX: 0, clientY: 200 });
    r.fire();
    place(0, 395, () => null);
    pointer("pointermove", "/a.jpg", { clientX: 0, clientY: 395 });
    expect(button("ghost").hidden).toBe(false);
    const before = list().scrollTop;
    r.fire();
    expect(list().scrollTop, "the list did not scroll under a held drag").toBeGreaterThan(before);
    // The folder scrolls under the finger, which has not moved.
    place(0, 395, () => row("/Photos"));
    r.fire();
    expect(row("/Photos").hasAttribute("data-drop"), "the folder under the finger is not a target").toBe(true);
    pointer("pointerup", "/a.jpg", { clientX: 0, clientY: 395 });
    await settle();
    expect(sent("files.move")).toEqual([{ paths: ["/a.jpg"], to: "/Photos" }]);
    // And it stops when the drag ends.
    const after = list().scrollTop;
    r.fire();
    expect(list().scrollTop).toBe(after);
  });

  it("scrolls up when held near the top edge", () => {
    list().getBoundingClientRect = () => ({ top: 100, bottom: 500, left: 0, right: 300, width: 300, height: 400, x: 0, y: 100 });
    Object.defineProperty(list(), "scrollHeight", { value: 4000, configurable: true });
    Object.defineProperty(list(), "clientHeight", { value: 400, configurable: true });
    list().scrollTop = 1000;
    pointer("pointerdown", "/a.jpg", { clientX: 0, clientY: 300 });
    r.fire();
    pointer("pointermove", "/a.jpg", { clientX: 0, clientY: 104 });
    r.fire();
    expect(list().scrollTop).toBeLessThan(1000);
    pointer("pointercancel", "/a.jpg", { clientX: 0, clientY: 104 });
  });

  it("a long-press that ends without a click leaves no state behind for the mouse", () => {
    // A touchscreen laptop: long-press with a finger, lift (no click comes),
    // tap Done, then use the touchpad.
    pointer("pointerdown", "/a.jpg");
    r.fire();
    pointer("pointerup", "/a.jpg");
    button("done").click();
    pointer("pointerdown", "/b.txt", { pointerType: "mouse", button: 2 });
    rightClick("/b.txt");
    expect(button("menu").hidden, "the right-click menu did not open").toBe(false);
    button("menu").dispatchEvent(new KeyboardEvent("keydown", { key: "Escape", bubbles: true }));
    pointer("pointerdown", "/a.jpg", { pointerType: "mouse" });
    pointer("pointerup", "/a.jpg", { pointerType: "mouse" });
    click("/a.jpg");
    expect(selectedPaths(), "the click was swallowed").toEqual(["/a.jpg"]);
  });

  it("is not offered in the trash", async () => {
    click("/a.jpg");
    key("Delete");
    await settle();
    button("trashview").click();
    await settle();
    pointer("pointerdown", "t0", { pointerType: "mouse" });
    pointer("pointermove", "t0", { pointerType: "mouse", clientX: 40, clientY: 40 });
    expect(button("ghost").hidden).toBe(true);
  });
});

// ---------------------------------------------------------------------------
// Open with
// ---------------------------------------------------------------------------

const menuLabels = () => [...button("menu").querySelectorAll(".menu-item")].map((b) => b.textContent);
function choose(label) {
  const item = [...button("menu").querySelectorAll(".menu-item")].find((b) => b.textContent === label);
  expect(item, `no "${label}" in ${menuLabels()}`).toBeTruthy();
  item.click();
}

describe("open with", () => {
  it("Enter on a text file opens it in the text editor when the desktop can", async () => {
    await remount({ desktop: true });
    click("/b.txt");
    key("Enter");
    expect(r.opened).toEqual([{ name: "scribe", path: "/b.txt" }]);
    expect(r.peeks).toEqual([]);
  });

  it("a picture still goes to Quick Look", async () => {
    await remount({ desktop: true });
    click("/a.jpg");
    key("Enter");
    expect(r.opened).toEqual([]);
    expect(r.peeks.map((p) => p.path)).toEqual(["/a.jpg"]);
  });

  it("without a desktop to ask, a text file falls back to Quick Look and the button is hidden", () => {
    click("/b.txt");
    key("Enter");
    expect(r.peeks.map((p) => p.path)).toEqual(["/b.txt"]);
    expect(button("openwith").hidden).toBe(true);
  });

  it("the Open with button offers the editor and Quick Look", async () => {
    await remount({ desktop: true });
    click("/a.jpg");
    expect(button("openwith").hidden).toBe(false);
    button("openwith").click();
    expect(menuLabels()).toEqual(["Text editor", "Quick Look"]);
    choose("Text editor");
    expect(r.opened).toEqual([{ name: "scribe", path: "/a.jpg" }]);
    expect(button("menu").hidden).toBe(true);
  });

  it("is not offered for a folder", async () => {
    await remount({ desktop: true });
    click("/Photos");
    expect(button("openwith").disabled).toBe(true);
  });

  it("says so when the desktop declines", async () => {
    await remount({ desktop: "refuses" });
    click("/b.txt");
    key("Enter");
    expect(statusText()).toContain("did not open");
  });

  it("a tap on a text file on a phone opens the editor too", async () => {
    await remount({ desktop: true });
    tap("/b.txt");
    expect(r.opened).toEqual([{ name: "scribe", path: "/b.txt" }]);
  });

  it("with no text editor installed, a text file opens in Quick Look and the editor is not offered", async () => {
    await remount({ desktop: "no-editor" });
    click("/b.txt");
    key("Enter");
    await settle();
    expect(r.peeks.map((p) => p.path), "Enter on a text file did nothing").toEqual(["/b.txt"]);
    expect(r.opened, "asked for an editor the desktop does not have").toEqual([]);
    expect(button("openwith").hidden, "Open with offered an editor that is not there").toBe(true);
    rightClick("/b.txt");
    expect(menuLabels()).not.toContain("Open with Text editor");
  });

  it("when the desktop refuses the editor after all, the file still opens, in Quick Look, and Files says why", async () => {
    await remount({ desktop: "stale" });
    click("/b.txt");
    key("Enter");
    await settle();
    expect(r.opened).toEqual([{ name: "scribe", path: "/b.txt" }]);
    expect(r.peeks.map((p) => p.path)).toEqual(["/b.txt"]);
    expect(statusText()).toContain("no app called scribe");
  });

  it("an Open with the desktop refuses says why", async () => {
    await remount({ desktop: "stale" });
    click("/a.jpg");
    button("openwith").click();
    choose("Text editor");
    await settle();
    expect(statusText()).toContain("no app called scribe");
  });
});

// ---------------------------------------------------------------------------
// The context menu
// ---------------------------------------------------------------------------

function rightClick(path) {
  row(path).dispatchEvent(
    new MouseEvent("contextmenu", { bubbles: true, cancelable: true, clientX: 5, clientY: 5 }),
  );
}

describe("the context menu", () => {
  it("right-click selects the row and offers what can be done to it", async () => {
    await remount({ desktop: true });
    rightClick("/b.txt");
    expect(selectedPaths()).toEqual(["/b.txt"]);
    expect(menuLabels()).toEqual([
      "Open",
      "Open with Text editor",
      "Quick Look",
      "Rename",
      "Cut",
      "Copy",
      "Move to the trash",
    ]);
  });

  it("acts on a choice and closes", () => {
    rightClick("/a.jpg");
    choose("Rename");
    expect(button("menu").hidden).toBe(true);
    expect(list().querySelector("input.rename")).toBeTruthy();
  });

  it("keeps a multiple selection when right-clicking inside it", () => {
    click("/a.jpg");
    click("/b.txt", { ctrlKey: true });
    rightClick("/b.txt");
    expect(selectedPaths()).toEqual(["/a.jpg", "/b.txt"]);
    expect(menuLabels()).toEqual(["Cut", "Copy", "Move to the trash"]);
  });

  it("Escape closes it", () => {
    rightClick("/a.jpg");
    button("menu").dispatchEvent(new KeyboardEvent("keydown", { key: "Escape", bubbles: true }));
    expect(button("menu").hidden).toBe(true);
  });

  it("opens from the keyboard on the row under the cursor", () => {
    key("ArrowDown");
    key("ContextMenu");
    expect(button("menu").hidden).toBe(false);
    expect(menuLabels()[0]).toBe("Open");
  });
});
