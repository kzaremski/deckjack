// @vitest-environment happy-dom
import { readFileSync } from "node:fs";
import { resolve } from "node:path";
import { beforeEach, describe, expect, it } from "vitest";

import * as cm from "../../../crates/deckjack-editor/ui/vendor/codemirror.js";
import {
  OPEN_MESSAGE,
  SESSION_KEY,
  WINDOW_TOTAL_MAX,
  WRAP_KEY,
  keyboardInset,
  languageFor,
  mountEditor,
  normalizePath,
} from "../../../crates/deckjack-editor/ui/editor.js";
import { APP_LAUNCH_MESSAGE } from "../shell/open-app.ts";
import { windowStorageKey } from "../stores/windows.ts";
import { tokenStylesheet } from "../tokens/css.ts";

/**
 * The Editor's UI, mounted from its real markup with the real vendored
 * CodeMirror and a fake node: a disk in a map, answering `editor.*`,
 * `files.stat` and `files.list` the way the core does (versions are content
 * hashes, a save against a stale version is a conflict, a new file never
 * overwrites).
 */

const html = readFileSync(resolve(process.cwd(), "../crates/deckjack-editor/ui/index.html"), "utf8");
const markup = html.slice(html.indexOf("<body>") + 6, html.indexOf("<script"));

/** Not the core's hash, only something that tells contents apart. */
const version = (text) => `v${text.length}:${[...text].reduce((h, c) => (h * 31 + c.charCodeAt(0)) | 0, 7)}`;

function node(initial = {}) {
  const disk = new Map(Object.entries(initial).map(([p, t]) => [p, { text: t, modified: 100 }]));
  const calls = [];
  const opened = (path, extra = {}) => {
    const f = disk.get(path);
    return {
      path: `r620:${path}`,
      name: path.slice(path.lastIndexOf("/") + 1),
      size: f.text.length,
      modified: f.modified,
      mode: "edit",
      reason: null,
      text: f.text,
      version: version(f.text),
      window: null,
      save_limit: 256 * 1024,
      ...extra,
      ...(f.open ?? {}),
    };
  };
  const strip = (p) => String(p).replace(/^r620:/, "");
  let clock = 100;
  const bus = {
    async invoke(command, params) {
      calls.push({ command, params });
      const path = strip(params.path);
      switch (command) {
        case "editor.open":
        case "editor.revert": {
          const f = disk.get(path);
          if (!f) throw new Error(`${path} does not exist`);
          if (f.binary) throw new Error(`${path}: is not text (it is not valid UTF-8), so the editor will not open it`);
          if (f.windows && params.before !== undefined) return f.windows(params.before);
          return opened(path);
        }
        case "editor.save": {
          const f = disk.get(path);
          const conflict =
            !params.force &&
            ((params.expected && (!f || version(f.text) !== params.expected)) || (!params.expected && f));
          if (conflict) {
            return {
              status: "conflict",
              path: `r620:${path}`,
              version: f ? version(f.text) : null,
              size: f?.text.length ?? null,
              modified: f?.modified ?? null,
              message: "the file changed on disk since it was opened; reload it, or save anyway",
            };
          }
          clock += 1;
          disk.set(path, { text: params.content, modified: clock });
          return {
            status: "saved",
            path: `r620:${path}`,
            version: version(params.content),
            size: params.content.length,
            modified: clock,
            message: null,
          };
        }
        case "files.stat": {
          const f = disk.get(path);
          if (!f) throw new Error("gone");
          return { path: `r620:${path}`, size: f.text.length, modified: f.modified };
        }
        case "files.list": {
          const dir = path === "/" ? "" : path;
          const names = new Map();
          for (const p of disk.keys()) {
            if (!p.startsWith(`${dir}/`)) continue;
            const rest = p.slice(dir.length + 1);
            const [head, ...tail] = rest.split("/");
            names.set(head, tail.length > 0);
          }
          return {
            path: `r620:${path}`,
            entries: [...names].map(([name, isDir]) => ({
              name,
              path: `r620:${dir}/${name}`,
              is_dir: isDir,
              kind: isDir ? "dir" : "file",
            })),
          };
        }
        default:
          throw new Error(`no such command ${command}`);
      }
    },
  };
  /** Someone else writing the file. */
  const external = (path, text) => {
    clock += 1;
    disk.set(path, { ...(disk.get(path) ?? {}), text, modified: clock });
  };
  return { bus, disk, calls, external };
}

function memory(seed = {}) {
  const map = new Map(Object.entries(seed));
  return { map, read: (k) => map.get(k) ?? null, write: (k, v) => void map.set(k, v) };
}

function rig({ files = { "/src/main.rs": "fn main() {}\n" }, storage = memory(), answers = {}, win, windowId } = {}) {
  document.body.innerHTML = markup;
  const n = node(files);
  const ticks = [];
  const asked = [];
  const app = mountEditor(document, {
    bus: n.bus,
    cm,
    storage,
    win,
    windowId,
    ask: async (q, v) => {
      asked.push(q);
      return answers.ask ?? v;
    },
    confirm: async (q) => {
      asked.push(q);
      return answers.confirm ?? false;
    },
    every: (_ms, fn) => {
      ticks.push(fn);
      return () => {};
    },
    later: (_ms, _fn) => () => {},
  });
  return { app, ...n, storage, asked, poll: async () => app.checkAll() };
}

const $ = (id) => document.getElementById(id);
const text = (app) => app.view.state.doc.toString();
const tabs = () => [...document.querySelectorAll("#tabs .tab-name")].map((t) => t.textContent);
const notice = () => ($("notice").hidden ? null : $("notice-text").textContent);
const noticeButton = (label) =>
  [...$("notice-actions").querySelectorAll("button")].find((b) => b.textContent === label);

/** Type at the end, as a person would, through CodeMirror's own input path. */
function type(app, s) {
  const end = app.view.state.doc.length;
  app.view.dispatch({ changes: { from: end, insert: s }, selection: { anchor: end + s.length }, userEvent: "input.type" });
}

// happy-dom fires `selectionchange` synchronously inside the selection call
// that caused it; browsers queue it as a task. CodeMirror relies on the
// browser's behavior (it moves the DOM selection during its own update), so
// the event is deferred here to match a browser.
const dispatch = document.dispatchEvent.bind(document);
document.dispatchEvent = (event) => {
  if (event.type === "selectionchange") {
    setTimeout(() => dispatch(event), 0);
    return true;
  }
  return dispatch(event);
};

let r;
beforeEach(() => {
  r?.app.destroy();
  r = undefined;
});

describe("opening", () => {
  it("opens the file the shell asked for and shows it", async () => {
    r = rig();
    await r.app.start("src/main.rs");
    expect(text(r.app)).toBe("fn main() {}\n");
    expect(tabs()).toEqual(["main.rs"]);
    expect(r.calls[0]).toEqual({ command: "editor.open", params: { path: "/src/main.rs" } });
    expect($("save").disabled, "nothing to save yet").toBe(true);
    expect($("position").textContent).toContain("Rust");
  });

  it("keeps several files open, each with its own text and history", async () => {
    r = rig({ files: { "/a.txt": "alpha", "/b.txt": "beta" } });
    const a = await r.app.open("/a.txt");
    type(r.app, " one");
    const b = await r.app.open("/b.txt");
    type(r.app, " two");
    expect(tabs()).toEqual(["● a.txt", "● b.txt"]);

    await r.app.open("/a.txt");
    expect(r.app.active).toBe(a);
    expect(text(r.app)).toBe("alpha one");
    cm.undo(r.app.view);
    expect(text(r.app), "undo reached into the other file").toBe("alpha");

    await r.app.open("r620:/b.txt");
    expect(r.app.active).toBe(b);
    expect(text(r.app)).toBe("beta two");
    expect(r.app.buffers).toHaveLength(2);
    // Switching is local: an open file is not read again (and its unsaved
    // text is not at the mercy of the connection).
    expect(r.calls.filter((c) => c.command === "editor.open")).toHaveLength(2);
  });

  it("keeps more than the text across a tab switch: the search panel stays where it was", async () => {
    r = rig({ files: { "/a.txt": "alpha", "/b.txt": "beta" } });
    await r.app.open("/a.txt");
    $("find").click();
    await r.app.open("/b.txt");
    expect(cm.searchPanelOpen(r.app.view.state)).toBe(false);
    await r.app.open("/a.txt");
    expect(cm.searchPanelOpen(r.app.view.state)).toBe(true);
  });

  it("opens what the hosting shell sends, and nothing from anywhere else", async () => {
    const parent = {};
    const listeners = {};
    const win = {
      parent,
      location: { origin: "https://node.example" },
      addEventListener: (t, fn) => (listeners[t] = fn),
      removeEventListener: () => {},
    };
    r = rig({ files: { "/a.txt": "alpha", "/b.txt": "beta" }, win });
    listeners.message({ source: {}, origin: "https://node.example", data: { type: OPEN_MESSAGE, path: "/b.txt" } });
    listeners.message({ source: parent, origin: "https://evil.example", data: { type: OPEN_MESSAGE, path: "/b.txt" } });
    await Promise.resolve();
    expect(r.app.buffers).toHaveLength(0);

    listeners.message({ source: parent, origin: "https://node.example", data: { type: OPEN_MESSAGE, path: "/a.txt" } });
    await new Promise((res) => setTimeout(res, 0));
    expect(r.app.buffers.map((b) => b.name)).toEqual(["a.txt"]);
  });

  it("offers Quick Look for a file that is not text", async () => {
    r = rig({ files: { "/p.jpg": "" } });
    r.disk.get("/p.jpg").binary = true;
    const peeks = [];
    // Remount with a shell.
    document.body.innerHTML = markup;
    const app = mountEditor(document, {
      bus: r.bus,
      cm,
      storage: memory(),
      shell: { quicklook: (p) => peeks.push(p) },
      ask: async () => null,
      confirm: async () => false,
      every: () => () => {},
      later: () => () => {},
    });
    expect(await app.open("/p.jpg")).toBeNull();
    expect(app.buffers).toHaveLength(0);
    noticeButton("Quick Look").click();
    expect(peeks).toEqual(["/p.jpg"]);
  });

  it("browses the working directory to pick a file", async () => {
    r = rig({ files: { "/src/main.rs": "fn main() {}", "/README.md": "# Hi" } });
    $("open").click();
    await new Promise((res) => setTimeout(res, 0));
    expect($("picker").hidden).toBe(false);
    const rows = () => [...$("picker-list").querySelectorAll(".dj-row")];
    rows().find((b) => b.textContent === "src/").click();
    await new Promise((res) => setTimeout(res, 0));
    rows().find((b) => b.textContent === "main.rs").click();
    await new Promise((res) => setTimeout(res, 0));
    expect($("picker").hidden).toBe(true);
    expect(r.app.active.name).toBe("main.rs");
  });
});

describe("saving", () => {
  it("has a Save button that writes against the version opened, and keeps undo across the save", async () => {
    r = rig({ files: { "/a.txt": "one" } });
    await r.app.open("/a.txt");
    type(r.app, " two");
    expect($("save").disabled).toBe(false);
    expect(tabs()).toEqual(["● a.txt"]);

    $("save").click();
    await new Promise((res) => setTimeout(res, 0));
    const save = r.calls.find((c) => c.command === "editor.save");
    expect(save.params).toEqual({ path: "r620:/a.txt", content: "one two", expected: version("one") });
    expect(r.disk.get("/a.txt").text).toBe("one two");
    expect(tabs()).toEqual(["a.txt"]);
    expect($("save").disabled).toBe(true);

    // The history is the buffer's, not the file's: a save does not cut it.
    cm.undo(r.app.view);
    expect(text(r.app)).toBe("one");
    expect(r.app.isDirty(r.app.active)).toBe(true);

    // And the next save names the version the last one produced.
    await r.app.save();
    const second = r.calls.filter((c) => c.command === "editor.save")[1];
    expect(second.params.expected).toBe(version("one two"));
  });

  it("saves with Ctrl+S from anywhere in the window", async () => {
    r = rig({ files: { "/a.txt": "one" } });
    await r.app.open("/a.txt");
    type(r.app, "!");
    document.dispatchEvent(new KeyboardEvent("keydown", { key: "s", ctrlKey: true, bubbles: true }));
    await new Promise((res) => setTimeout(res, 0));
    expect(r.disk.get("/a.txt").text).toBe("one!");
    expect(r.calls.filter((c) => c.command === "editor.save")).toHaveLength(1);
  });

  it("does not overwrite a change made on disk; the operator chooses", async () => {
    r = rig({ files: { "/a.txt": "mine" } });
    await r.app.open("/a.txt");
    type(r.app, " edit");
    r.external("/a.txt", "theirs");

    expect(await r.app.save()).toBe(false);
    expect(r.disk.get("/a.txt").text).toBe("theirs");
    expect(notice()).toContain("changed on disk");

    noticeButton("Save anyway").click();
    await new Promise((res) => setTimeout(res, 0));
    expect(r.disk.get("/a.txt").text).toBe("mine edit");
    expect(notice()).toBeNull();
  });

  it("can take their version instead, as one change that can be undone", async () => {
    r = rig({ files: { "/a.txt": "mine" } });
    await r.app.open("/a.txt");
    type(r.app, " edit");
    r.external("/a.txt", "theirs");
    await r.app.save();
    noticeButton("Reload theirs").click();
    await new Promise((res) => setTimeout(res, 0));
    expect(text(r.app)).toBe("theirs");
    expect(r.app.isDirty(r.app.active)).toBe(false);
    cm.undo(r.app.view);
    expect(text(r.app), "the revert cost the edit history").toBe("mine edit");
  });

  it("asks where to save a new file, and never overwrites one that exists", async () => {
    r = rig({ files: { "/taken.txt": "keep" }, answers: { ask: "notes/new.md" } });
    r.app.newBuffer();
    type(r.app, "# New");
    expect(await r.app.save()).toBe(true);
    const save = r.calls.find((c) => c.command === "editor.save");
    expect(save.params).toEqual({ path: "/notes/new.md", content: "# New" });
    expect(r.app.active.name).toBe("new.md");
    expect($("position").textContent).toContain("Markdown");

    r.app.newBuffer();
    type(r.app, "clobber");
    // A second new file aimed at an existing name is a conflict, not a write.
    r.disk.set("/notes/new.md", { text: "keep", modified: 1 });
    expect(await r.app.save()).toBe(false);
    expect(r.disk.get("/notes/new.md").text).toBe("keep");
  });

  it("refuses to send a save bigger than the socket carries, and says so", async () => {
    r = rig({ files: { "/a.txt": "x" } });
    await r.app.open("/a.txt");
    r.app.active.saveLimit = 64;
    type(r.app, "y".repeat(100));
    expect(await r.app.save()).toBe(false);
    expect(r.calls.some((c) => c.command === "editor.save")).toBe(false);
    expect($("status").textContent).toContain("more than a save can carry");
  });
});

describe("a change on disk while a file is open", () => {
  it("reloads a file with no unsaved changes", async () => {
    r = rig({ files: { "/a.txt": "old" } });
    await r.app.open("/a.txt");
    r.external("/a.txt", "new");
    await r.poll();
    expect(text(r.app)).toBe("new");
    expect(r.app.isDirty(r.app.active)).toBe(false);
  });

  it("tells the operator, and keeps their text, when there are unsaved changes", async () => {
    r = rig({ files: { "/a.txt": "old" } });
    await r.app.open("/a.txt");
    type(r.app, " mine");
    r.external("/a.txt", "new");
    await r.poll();
    expect(text(r.app)).toBe("old mine");
    expect(notice()).toContain("changed on disk");
    expect(noticeButton("Reload theirs")).toBeTruthy();
  });

  it("is not fooled by its own save", async () => {
    r = rig({ files: { "/a.txt": "old" } });
    await r.app.open("/a.txt");
    type(r.app, " mine");
    await r.app.save();
    const before = r.calls.length;
    await r.poll();
    expect(r.calls.slice(before).map((c) => c.command)).toEqual(["files.stat"]);
    expect(notice()).toBeNull();
  });
});

describe("big files", () => {
  it("open read-only, say why, and offer nothing to save", async () => {
    r = rig({ files: { "/big.txt": "lots" } });
    r.disk.get("/big.txt").open = { mode: "read_only", reason: "800 KB is more than can be saved back" };
    await r.app.open("/big.txt");
    expect(r.app.view.state.readOnly).toBe(true);
    expect(notice()).toContain("more than can be saved back");
    expect($("save").disabled).toBe(true);
    expect(await r.app.save()).toBe(false);
  });

  it("show the tail first and page backwards on request", async () => {
    r = rig({ files: { "/app.log": "c\n" } });
    const f = r.disk.get("/app.log");
    f.open = { mode: "read_only", reason: "too large", version: null, window: { start: 20, end: 22 } };
    f.windows = (before) => ({
      ...f.open,
      text: before === 20 ? "b\n" : "a\n",
      window: before === 20 ? { start: 10, end: 20 } : { start: 0, end: 10 },
    });
    await r.app.open("/app.log");
    expect($("earlier").hidden).toBe(false);
    $("earlier").click();
    await new Promise((res) => setTimeout(res, 0));
    expect(text(r.app)).toBe("b\nc\n");
    await r.app.loadEarlier();
    expect(text(r.app)).toBe("a\nb\nc\n");
    expect($("earlier").hidden, "still offered at the start of the file").toBe(true);
    // A window is never polled for changes: it is not the file.
    const before = r.calls.length;
    await r.poll();
    expect(r.calls.length).toBe(before);
  });
});

describe("without a hardware keyboard", () => {
  it("find and replace, go to line, undo and redo are buttons", async () => {
    r = rig({ files: { "/a.txt": "one\ntwo\nthree" } });
    await r.app.open("/a.txt");
    $("find").click();
    expect(cm.searchPanelOpen(r.app.view.state)).toBe(true);
    expect(document.querySelector(".cm-search [name=replace]"), "no replace field").toBeTruthy();
    $("find").click();
    expect(cm.searchPanelOpen(r.app.view.state)).toBe(false);

    $("goto").click();
    expect(document.querySelector(".cm-panel input"), "no go-to-line field").toBeTruthy();

    type(r.app, "!");
    expect($("undo").disabled).toBe(false);
    $("undo").click();
    expect(text(r.app)).toBe("one\ntwo\nthree");
    $("redo").click();
    expect(text(r.app)).toBe("one\ntwo\nthree!");
  });

  it("wraps long lines by default and remembers the choice", async () => {
    const storage = memory();
    r = rig({ storage });
    await r.app.open("/src/main.rs");
    const wrapped = () => document.querySelector(".cm-content").classList.contains("cm-lineWrapping");
    expect(wrapped()).toBe(true);
    $("wrap").click();
    expect(wrapped()).toBe(false);
    expect($("wrap").getAttribute("aria-pressed")).toBe("false");
    expect(storage.map.get(WRAP_KEY)).toBe("off");
  });

  it("closing a file with unsaved changes asks first", async () => {
    r = rig({ files: { "/a.txt": "one" }, answers: { confirm: false } });
    const b = await r.app.open("/a.txt");
    type(r.app, "!");
    expect(await r.app.close(b)).toBe(false);
    expect(r.app.buffers).toHaveLength(1);
    expect(r.asked.at(-1)).toContain("unsaved");
  });
});

describe("a reload of the page", () => {
  it("brings back open files and unsaved text, saved against the version it was based on", async () => {
    const storage = memory();
    r = rig({ files: { "/a.txt": "one", "/b.txt": "two" }, storage });
    await r.app.open("/a.txt");
    await r.app.open("/b.txt");
    type(r.app, " unsaved");
    r.app.destroy();
    expect(JSON.parse(storage.map.get(SESSION_KEY)).buffers).toHaveLength(2);

    // Same disk, fresh page; and the file moved on while the page was gone.
    const disk = r.disk;
    r = rig({ files: { "/a.txt": "one", "/b.txt": "two" }, storage });
    for (const [k, v] of disk) r.disk.set(k, v);
    r.external("/b.txt", "changed elsewhere");
    await r.app.start(null);
    expect(r.app.buffers.map((b) => b.name)).toEqual(["a.txt", "b.txt"]);
    expect(r.app.active.name).toBe("b.txt");
    expect(text(r.app)).toBe("two unsaved");
    expect(r.app.isDirty(r.app.active)).toBe(true);

    // The draft was made against "two", not against what is there now, so
    // saving it is a conflict and not a silent overwrite.
    expect(await r.app.save()).toBe(false);
    expect(r.disk.get("/b.txt").text).toBe("changed elsewhere");
  });

  it("does not forget the rest of the session while it is still bringing it back", async () => {
    const record = {
      active: "r620:/a.txt",
      buffers: [
        { path: "r620:/a.txt", cursor: 0, draft: "one edited", version: version("one") },
        { path: "r620:/slow.txt", cursor: 0 },
      ],
    };
    const storage = memory({ [SESSION_KEY]: JSON.stringify(record) });
    r = rig({ files: { "/a.txt": "one", "/slow.txt": "slow" }, storage });
    // The second file's answer never comes, as on a connection that dropped.
    const invoke = r.bus.invoke;
    r.bus.invoke = (command, params) =>
      String(params.path).endsWith("slow.txt") ? new Promise(() => {}) : invoke(command, params);
    void r.app.start(null);
    await new Promise((res) => setTimeout(res, 0));
    expect(text(r.app)).toBe("one edited");
    const kept = JSON.parse(storage.map.get(SESSION_KEY));
    expect(kept.buffers.map((b) => b.path)).toEqual(["r620:/a.txt", "r620:/slow.txt"]);
  });
});

describe("the pieces", () => {
  it("picks a language by name, and plain text when it does not know", () => {
    expect(languageFor("main.rs", cm)).not.toBeNull();
    expect(languageFor("Dockerfile", cm)).not.toBeNull();
    expect(languageFor("hyprland.lua", cm)).not.toBeNull();
    expect(languageFor("notes.txt", cm)).toBeNull();
    expect(languageFor("LICENSE", cm)).toBeNull();
  });

  it("measures how much of the frame the keyboard covers", () => {
    expect(keyboardInset({ frameBottom: 800, viewportTop: 0, viewportHeight: 500 })).toBe(300);
    expect(keyboardInset({ frameBottom: 400, viewportTop: 0, viewportHeight: 500 })).toBe(0);
    expect(keyboardInset({ frameBottom: 800, viewportTop: 100, viewportHeight: 500 })).toBe(200);
    expect(keyboardInset({ frameBottom: NaN, viewportTop: 0, viewportHeight: 1 })).toBe(0);
  });

  it("re-pads the view when the visual viewport changes, without being asked", () => {
    const heard = {};
    const vv = {
      offsetTop: 0,
      height: 760,
      addEventListener: (t, fn) => (heard[t] = fn),
      removeEventListener() {},
    };
    const win = {
      parent: { visualViewport: vv },
      location: { origin: "x" },
      frameElement: { getBoundingClientRect: () => ({ bottom: 760 }) },
      addEventListener() {},
      removeEventListener() {},
    };
    r = rig({ win });
    const inset = () => document.documentElement.style.getPropertyValue("--keyboard-inset");
    // The keyboard opens: the visual viewport shrinks, and says so.
    vv.height = 500;
    heard.resize?.();
    expect(inset()).toBe("260px");
    // The page scrolls under it: the viewport moves, and says so.
    vv.offsetTop = 100;
    heard.scroll?.();
    expect(inset()).toBe("160px");
  });

  it("pads the view by that much when the keyboard opens", () => {
    const vv = { offsetTop: 0, height: 500, addEventListener() {}, removeEventListener() {} };
    const parent = { visualViewport: vv };
    const win = {
      parent,
      location: { origin: "x" },
      frameElement: { getBoundingClientRect: () => ({ bottom: 760 }) },
      addEventListener() {},
      removeEventListener() {},
    };
    r = rig({ win });
    r.app.fitKeyboard();
    expect(document.documentElement.style.getPropertyValue("--keyboard-inset")).toBe("260px");
  });

  it("listens for the message the shell posts into an open window", () => {
    expect(OPEN_MESSAGE).toBe(APP_LAUNCH_MESSAGE);
  });

  it("makes a path from the shell into a file address", () => {
    expect(normalizePath("src/main.rs")).toBe("/src/main.rs");
    expect(normalizePath("/src/main.rs")).toBe("/src/main.rs");
    expect(normalizePath("r620:/a")).toBe("r620:/a");
    expect(normalizePath("")).toBeNull();
    expect(normalizePath("a\0b")).toBeNull();
    expect(normalizePath(7)).toBeNull();
  });
});

const tick = () => new Promise((res) => setTimeout(res, 0));

describe("a change on disk, the edge cases", () => {
  it("does not overwrite typing that happens while it is reloading", async () => {
    r = rig({ files: { "/a.txt": "old" } });
    await r.app.open("/a.txt");
    r.external("/a.txt", "new");
    let release;
    const gate = new Promise((res) => (release = res));
    const invoke = r.bus.invoke;
    r.bus.invoke = async (command, params) => {
      const value = await invoke(command, params);
      if (command === "editor.revert") await gate;
      return value;
    };
    const polled = r.poll();
    await tick();
    type(r.app, " typed");
    release();
    await polled;
    expect(text(r.app)).toBe("old typed");
    expect(r.app.isDirty(r.app.active)).toBe(true);
    expect(notice()).toContain("changed on disk");
  });

  it("does not mistake its own save, still in flight, for someone else's write", async () => {
    r = rig({ files: { "/a.txt": "old" } });
    await r.app.open("/a.txt");
    type(r.app, " mine");
    let release;
    const gate = new Promise((res) => (release = res));
    const invoke = r.bus.invoke;
    r.bus.invoke = async (command, params) => {
      const value = await invoke(command, params);
      if (command === "editor.save") await gate;
      return value;
    };
    const saving = r.app.save();
    await tick();
    await r.poll();
    expect(notice()).toBeNull();
    release();
    expect(await saving).toBe(true);
    expect(notice()).toBeNull();
  });

  it("notices a write in the same second by its size", async () => {
    r = rig({ files: { "/a.txt": "old" } });
    await r.app.open("/a.txt");
    const same = r.disk.get("/a.txt").modified;
    r.disk.set("/a.txt", { text: "newer", modified: same });
    await r.poll();
    expect(text(r.app)).toBe("newer");
  });

  it("makes a reloaded file read-only when it has grown too big to save back", async () => {
    r = rig({ files: { "/a.txt": "small" } });
    await r.app.open("/a.txt");
    r.external("/a.txt", "now enormous");
    r.disk.get("/a.txt").open = { mode: "read_only", reason: "too large to save back" };
    await r.poll();
    expect(text(r.app)).toBe("now enormous");
    expect(r.app.view.state.readOnly).toBe(true);
    expect($("save").disabled).toBe(true);
  });

  it("checks open files when the window gets focus", async () => {
    const listeners = {};
    const win = {
      parent: {},
      location: { origin: "https://node.example" },
      addEventListener: (t, fn) => (listeners[t] = fn),
      removeEventListener: () => {},
    };
    r = rig({ files: { "/a.txt": "old" }, win });
    await r.app.open("/a.txt");
    r.external("/a.txt", "new");
    listeners.focus();
    await tick();
    await tick();
    expect(text(r.app)).toBe("new");
  });

  it("stops paging back once the view holds as much as it may", async () => {
    r = rig({ files: { "/app.log": "tail\n" } });
    r.disk.get("/app.log").open = {
      mode: "read_only",
      reason: "too large",
      version: null,
      window: { start: 10, end: 10 + WINDOW_TOTAL_MAX },
    };
    r.disk.get("/app.log").windows = () => ({ text: "more\n", window: { start: 0, end: 10 } });
    await r.app.open("/app.log");
    expect(await r.app.loadEarlier()).toBe(false);
    expect(r.calls.some((c) => c.params.before !== undefined)).toBe(false);
    expect($("status").textContent).toContain("as much as this view holds");
  });
});

describe("saving a new file over one that exists", () => {
  it("offers to open the file that is there, and opens it", async () => {
    r = rig({ files: { "/taken.txt": "keep" }, answers: { ask: "/taken.txt" } });
    r.app.newBuffer();
    type(r.app, "mine");
    expect(await r.app.save()).toBe(false);
    expect(noticeButton("Reload theirs"), "a reload of a file this buffer never had").toBeUndefined();
    noticeButton("Open it").click();
    await tick();
    expect(r.app.active.name).toBe("taken.txt");
    expect(text(r.app)).toBe("keep");
    expect(r.app.buffers.some((b) => b.path === null && r.app.isDirty(b)), "the new text was lost").toBe(true);
  });

  it("saves over it without asking for the path again", async () => {
    r = rig({ files: { "/taken.txt": "keep" }, answers: { ask: "/taken.txt" } });
    r.app.newBuffer();
    type(r.app, "mine");
    await r.app.save();
    noticeButton("Save anyway").click();
    await tick();
    expect(r.asked).toHaveLength(1);
    expect(r.disk.get("/taken.txt").text).toBe("mine");
    expect(r.app.active.name).toBe("taken.txt");
    expect(notice()).toBeNull();
  });
});

describe("unsaved text kept on this device", () => {
  it("is kept under the key the shell clears when the window closes", async () => {
    const storage = memory();
    r = rig({ files: { "/a.txt": "one" }, storage, windowId: "w9" });
    await r.app.open("/a.txt");
    type(r.app, " unsaved");
    const kept = JSON.parse(storage.map.get(windowStorageKey("w9")));
    expect(kept.buffers[0].draft).toBe("one unsaved");
  });

  it("says so when it could not be kept", async () => {
    const storage = memory();
    storage.write = () => false;
    r = rig({ files: { "/a.txt": "one" }, storage });
    await r.app.open("/a.txt");
    type(r.app, " unsaved");
    expect($("status").dataset.kind).toBe("error");
    expect($("status").textContent).toContain("could not be kept");
  });
});

describe("the page's colors", () => {
  it("names only tokens the shell defines, so a light theme is not painted dark", () => {
    const sources = ["editor.js", "index.html"].map((f) =>
      readFileSync(resolve(process.cwd(), `../crates/deckjack-editor/ui/${f}`), "utf8"),
    );
    const defined = new Set(tokenStylesheet().match(/--dj-[a-z0-9-]+(?=:)/g));
    const used = new Set(sources.flatMap((src) => src.match(/var\((--dj-[a-z0-9-]+)/g) ?? []).map((v) => v.slice(4)));
    expect(used.size).toBeGreaterThan(10);
    expect([...used].filter((name) => !defined.has(name))).toEqual([]);
  });
});
