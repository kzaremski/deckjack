// @vitest-environment happy-dom
// @vitest-environment-options {"settings":{"navigation":{"disableChildFrameNavigation":true}}}
import { flushPromises } from "@vue/test-utils";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { nextTick, reactive } from "vue";

import AppFramePanel from "../components/panels/AppFramePanel.vue";
import { harness, HARNESS_ORIGIN, mountInShell } from "../testing/shell-harness.ts";
import type { ShellWindow } from "../stores/windows.ts";
import { APP_LAUNCH_MESSAGE, frameSrc, parseAppOpen } from "./open-app.ts";

/**
 * The open-app contract, end to end on the shell's side: a frame the shell
 * hosts posts `{ type: "shell.app.open", app, path }` (what `/kit/shell.js`'s
 * `openApp` sends), and the named app's window opens or comes forward with
 * the path. Real shell stores; the frame is any object the bridge accepts.
 */

function bridged(options: { installed?: string[] } = {}) {
  const rig = harness();
  rig.connect();
  if (options.installed) {
    rig.shell.apps.installed.value = options.installed.map((name) => ({
      name,
      title: name,
      version: "0.1.0",
      lifecycle: "on-demand",
      status: "live",
      compact: true,
      wide: true,
    }));
  }
  const frame = {};
  rig.shell.frames.attach(frame);
  const post = (data: unknown, overrides: { origin?: string; source?: unknown } = {}) =>
    rig.shell.frames.handle({
      origin: overrides.origin ?? HARNESS_ORIGIN,
      source: "source" in overrides ? overrides.source : frame,
      data,
    });
  return { rig, post };
}

const editorWindows = (rig: ReturnType<typeof harness>) =>
  rig.shell.windows.all.value.filter((w) => w.appId === "editor");

describe("a frame asking for an app on a file", () => {
  it("opens that app's window with the path", async () => {
    const { rig, post } = bridged({ installed: ["editor"] });
    expect(post({ type: "shell.app.open", app: "editor", path: "src/main.rs" })).toBe(true);
    await flushPromises();
    const [w] = editorWindows(rig);
    expect(w, "no editor window opened").toBeTruthy();
    expect(w!.launch?.path).toBe("/src/main.rs");
    expect(rig.shell.windows.focused.value?.id).toBe(w!.id);
  });

  it("brings an open window forward and hands it the new path, rather than opening another", async () => {
    const { rig, post } = bridged({ installed: ["editor"] });
    post({ type: "shell.app.open", app: "editor", path: "/a.txt" });
    await flushPromises();
    const [first] = editorWindows(rig);
    const seq = first!.launch!.seq;
    rig.shell.windows.minimize(first!.id);

    post({ type: "shell.app.open", app: "editor", path: "/b.txt" });
    await flushPromises();
    const all = editorWindows(rig);
    expect(all).toHaveLength(1);
    expect(all[0]!.minimized).toBe(false);
    expect(all[0]!.launch?.path).toBe("/b.txt");
    expect(all[0]!.launch!.seq).not.toBe(seq);
    expect(rig.shell.windows.focused.value?.id).toBe(first!.id);

    // The same file twice is still a request the app should hear.
    const before = all[0]!.launch!.seq;
    post({ type: "shell.app.open", app: "editor", path: "/b.txt" });
    await flushPromises();
    expect(editorWindows(rig)[0]!.launch!.seq).not.toBe(before);
  });

  it("may name no file, which only brings the app forward", async () => {
    const { rig, post } = bridged({ installed: ["editor"] });
    expect(post({ type: "shell.app.open", app: "editor" })).toBe(true);
    await flushPromises();
    expect(editorWindows(rig)).toHaveLength(1);
    expect(editorWindows(rig)[0]!.launch ?? null).toBeNull();
  });

  it("is ignored for an app the shell does not know, or a path that is not a path", async () => {
    const { rig, post } = bridged({ installed: ["editor"] });
    for (const data of [
      { type: "shell.app.open", app: "nonesuch", path: "/a" },
      { type: "shell.app.open", app: "", path: "/a" },
      { type: "shell.app.open", path: "/a" },
      { type: "shell.app.open", app: "editor", path: "a\0b" },
      { type: "shell.app.open", app: "editor", path: 7 },
      { type: "shell.app.open", app: "editor", path: "" },
      { type: "shell.app.open", app: "editor", path: "../../etc/passwd" },
    ]) {
      expect(post(data), JSON.stringify(data)).toBe(false);
    }
    await flushPromises();
    expect(rig.shell.windows.all.value).toHaveLength(0);
  });

  it("is ignored from another origin or from a window the shell is not hosting", async () => {
    const { rig, post } = bridged({ installed: ["editor"] });
    const ask = { type: "shell.app.open", app: "editor", path: "/a" };
    expect(post(ask, { origin: "https://elsewhere.example" })).toBe(false);
    expect(post(ask, { source: {} })).toBe(false);
    expect(post(ask, { source: null })).toBe(false);
    await flushPromises();
    expect(rig.shell.windows.all.value).toHaveLength(0);
  });

  it("is the same rule from the palette or an agent, which get told why", async () => {
    const { rig } = bridged({ installed: ["editor"] });
    await rig.shell.commands.dispatch("shell.app.open", { app: "nonesuch", path: "/a" });
    expect(rig.shell.commands.lastError.value).toContain("no app called nonesuch");
    // Not awaited: opening a window waits for the node to record it, and
    // this fake node never answers. The window exists before that.
    void rig.shell.commands.dispatch("shell.app.open", { app: "editor", path: "notes/x.md" });
    await flushPromises();
    expect(editorWindows(rig)[0]!.launch?.path).toBe("/notes/x.md");
  });
});

describe("/kit/shell.js", () => {
  afterEach(() => {
    vi.restoreAllMocks();
  });

  it("posts exactly what the shell accepts", async () => {
    const posted: Array<{ data: unknown; origin: string }> = [];
    const parent = { postMessage: (data: unknown, origin: string) => posted.push({ data, origin }) };
    vi.spyOn(window, "parent", "get").mockReturnValue(parent as unknown as Window);
    // @ts-expect-error -- a plain JS module served from public/
    const { shell } = await import("../../public/kit/shell.js");

    // A promise of the shell's answer (this stand-in parent never answers).
    expect(shell.openApp("editor", { path: "/src/main.rs" })).toBeInstanceOf(Promise);
    expect(posted).toHaveLength(1);
    expect(posted[0]!.origin).toBe(location.origin);
    expect((window as unknown as { deckjackShell?: unknown }).deckjackShell).toBe(shell);

    // And what it posted, handed to a real shell's bridge, opens the editor.
    const { rig, post } = bridged({ installed: ["editor"] });
    expect(post(posted[0]!.data)).toBe(true);
    await flushPromises();
    expect(editorWindows(rig)[0]!.launch?.path).toBe("/src/main.rs");
  });
});

describe("an app window opened on a file", () => {
  // The panel asks for the app's page before loading it (see
  // AppFramePanel.test.ts); here the app is always live.
  beforeEach(() => {
    vi.stubGlobal("fetch", vi.fn(async () => new Response("<!doctype html>", { status: 200 })));
  });
  afterEach(() => {
    vi.unstubAllGlobals();
    vi.restoreAllMocks();
  });

  function windowFor(launch: ShellWindow["launch"]) {
    return reactive({
      id: "w7",
      app: "editor",
      appId: "editor",
      title: "Editor",
      launch,
    }) as unknown as ShellWindow;
  }

  it("loads its frame with the path and its window id", async () => {
    const rig = harness();
    const { wrapper } = mountInShell(AppFramePanel, {
      harness: rig,
      props: { window: windowFor({ path: "/src/main.rs", seq: 1 }) },
      attachTo: document.body,
    });
    await flushPromises();
    const src = wrapper.find("iframe").attributes("src")!;
    const url = new URL(src, "https://x.example");
    expect(url.pathname).toBe("/apps/editor/");
    expect(url.searchParams.get("path")).toBe("/src/main.rs");
    expect(url.searchParams.get("window")).toBe("w7");
    wrapper.unmount();
  });

  it("is told about a later file by message, without reloading", async () => {
    const rig = harness();
    const w = windowFor({ path: "/a.txt", seq: 1 });
    const { wrapper } = mountInShell(AppFramePanel, {
      harness: rig,
      props: { window: w },
      attachTo: document.body,
    });
    await flushPromises();
    const iframe = wrapper.find("iframe").element as HTMLIFrameElement;
    const src = iframe.getAttribute("src");
    const sent: Array<{ data: unknown; origin: string }> = [];
    vi.spyOn(iframe.contentWindow!, "postMessage").mockImplementation(((data: unknown, origin: string) => {
      sent.push({ data, origin });
    }) as typeof window.postMessage);

    iframe.dispatchEvent(new Event("load"));
    expect(sent, "the path it loaded with was sent again").toHaveLength(0);
    w.launch = { path: "/b.txt", seq: 2 };
    await nextTick();
    expect(sent).toEqual([{ data: { type: APP_LAUNCH_MESSAGE, path: "/b.txt" }, origin: location.origin }]);

    w.launch = { path: "/c.txt", seq: 3 };
    await nextTick();
    expect(sent.at(-1)!.data).toEqual({ type: APP_LAUNCH_MESSAGE, path: "/c.txt" });
    expect(iframe.getAttribute("src"), "the frame reloaded").toBe(src);
    wrapper.unmount();
  });

  it("holds a file asked for before the frame has loaded, and hands it over on load", async () => {
    // happy-dom fires an iframe's load on its own schedule; a real app
    // frame can take seconds. Only the load this test sends gets through.
    const ours = new Event("load");
    const real = HTMLIFrameElement.prototype.dispatchEvent;
    vi.spyOn(HTMLIFrameElement.prototype, "dispatchEvent").mockImplementation(function (
      this: HTMLIFrameElement,
      event: Event,
    ) {
      if (event.type === "load" && event !== ours) return true;
      return real.call(this, event);
    });
    const rig = harness();
    const w = windowFor({ path: "/a.txt", seq: 1 });
    const { wrapper } = mountInShell(AppFramePanel, {
      harness: rig,
      props: { window: w },
      attachTo: document.body,
    });
    const iframe = wrapper.find("iframe").element as HTMLIFrameElement;
    const sent: unknown[] = [];
    vi.spyOn(iframe.contentWindow!, "postMessage").mockImplementation(((data: unknown) => {
      sent.push(data);
    }) as typeof window.postMessage);

    // A second request arrives while the app is still starting.
    w.launch = { path: "/b.txt", seq: 2 };
    await nextTick();
    expect(sent, "posted into a frame that had not loaded").toHaveLength(0);

    await flushPromises();
    iframe.dispatchEvent(ours);
    expect(sent).toEqual([{ type: APP_LAUNCH_MESSAGE, path: "/b.txt" }]);
    wrapper.unmount();
  });

  it("hands a file sent into the frame over again when the frame reloads", async () => {
    // An app that restarts under an open window comes back through the
    // core's "restarting" page, which reloads itself until the app is up,
    // and the reload goes back to the URL the window was opened with.
    // Without handing the last file over again, the app comes up on the
    // first file, not the one the operator asked for last.
    const loads: Event[] = [];
    const real = HTMLIFrameElement.prototype.dispatchEvent;
    vi.spyOn(HTMLIFrameElement.prototype, "dispatchEvent").mockImplementation(function (
      this: HTMLIFrameElement,
      event: Event,
    ) {
      if (event.type === "load" && !loads.includes(event)) return true;
      return real.call(this, event);
    });
    const load = (iframe: HTMLIFrameElement) => {
      const event = new Event("load");
      loads.push(event);
      iframe.dispatchEvent(event);
    };
    const rig = harness();
    const w = windowFor({ path: "/a.txt", seq: 1 });
    const { wrapper } = mountInShell(AppFramePanel, {
      harness: rig,
      props: { window: w },
      attachTo: document.body,
    });
    await flushPromises();
    const iframe = wrapper.find("iframe").element as HTMLIFrameElement;
    const src = iframe.getAttribute("src");
    const sent: unknown[] = [];
    vi.spyOn(iframe.contentWindow!, "postMessage").mockImplementation(((data: unknown) => {
      sent.push(data);
    }) as typeof window.postMessage);

    // The app loads, and a second file is handed over by message.
    load(iframe);
    w.launch = { path: "/b.txt", seq: 2 };
    await nextTick();
    expect(sent).toEqual([{ type: APP_LAUNCH_MESSAGE, path: "/b.txt" }]);

    // The same document's load again is not a reload: nothing is sent.
    load(iframe);
    expect(sent).toHaveLength(1);

    // The app restarts: the frame reloads the window's URL, which still
    // names a.txt, into a new document.
    const reloaded = document.implementation.createHTMLDocument("reloaded");
    vi.spyOn(iframe, "contentDocument", "get").mockReturnValue(reloaded);
    load(iframe);
    expect(iframe.getAttribute("src")).toBe(src);
    expect(sent).toEqual([
      { type: APP_LAUNCH_MESSAGE, path: "/b.txt" },
      { type: APP_LAUNCH_MESSAGE, path: "/b.txt" },
    ]);
    wrapper.unmount();
  });

  it("follows the window it is handed, as the compact shell's one panel is", async () => {
    // The compact shell renders the focused window in a single panel and
    // swaps the prop; a frame URL fixed at mount kept showing the old app.
    const rig = harness();
    const files = reactive({ id: "w1", app: "files", appId: "files", title: "Files" }) as unknown as ShellWindow;
    const { wrapper } = mountInShell(AppFramePanel, {
      harness: rig,
      props: { window: files },
      attachTo: document.body,
    });
    await wrapper.setProps({ window: windowFor({ path: "/README.md", seq: 4 }) });
    await flushPromises();
    const url = new URL(wrapper.find("iframe").attributes("src")!, "https://x.example");
    expect(url.pathname).toBe("/apps/editor/");
    expect(url.searchParams.get("path")).toBe("/README.md");
    expect(url.searchParams.get("window")).toBe("w7");
    wrapper.unmount();
  });
});

describe("the pieces", () => {
  const apps = { isKnown: (id: string) => id === "editor" };

  it("normalizes a path to an address and keeps a node's", () => {
    expect(parseAppOpen({ app: "editor", path: "a/b" }, apps)).toEqual({
      ok: true,
      request: { app: "editor", path: "/a/b" },
    });
    expect(parseAppOpen({ app: "editor", path: "r620:/a" }, apps)).toEqual({
      ok: true,
      request: { app: "editor", path: "r620:/a" },
    });
    expect(parseAppOpen({ app: "editor", path: "r620:/a/../b" }, apps).ok).toBe(false);
    expect(parseAppOpen(null, apps).ok).toBe(false);
    expect(parseAppOpen(["editor"], apps).ok).toBe(false);
  });

  it("builds a frame URL an app can read back", () => {
    const url = new URL(frameSrc("editor", "w1", "/a b&c.txt"), "https://x.example");
    expect(url.searchParams.get("path")).toBe("/a b&c.txt");
    expect(new URL(frameSrc("files", "w2"), "https://x.example").search).toBe("?window=w2");
  });
});

describe("what a frame may open", () => {
  it("is an installed app, never a core panel such as the terminal or the Assistant", async () => {
    const { rig, post } = bridged({ installed: ["editor"] });
    for (const app of ["terminal", "assistant", "notes"]) {
      expect(post({ type: "shell.app.open", app }), `a frame opened ${app}`).toBe(false);
      expect(post({ type: "shell.app.open", app, path: "/a.txt" }), `a frame opened ${app}`).toBe(false);
    }
    await flushPromises();
    expect(rig.shell.windows.all.value.map((w) => w.appId)).toEqual([]);
    // The palette and an agent still reach the core panels the same way.
    void rig.shell.commands.dispatch("shell.app.open", { app: "terminal" });
    await flushPromises();
    expect(rig.shell.windows.all.value.map((w) => w.appId)).toEqual(["terminal"]);
  });
});

describe("a path handed to an app", () => {
  // Every byte of a name is the name: the core does not trim, so '/t.md '
  // and '/t.md' are two files, and opening one must not open the other.
  const names = [
    "/notes.md ",
    " leading.md",
    "/a#b?c%20d.txt",
    "/dir/ü ñ 日本.txt",
    "r620:/x/100% #1?.md",
    "/plus+sign.txt",
  ];

  it("reaches the app exactly as Files sent it, through the shell and the frame URL", async () => {
    // @ts-expect-error -- a plain JS module, the editor's own UI
    const { normalizePath } = await import("../../../crates/deckjack-editor/ui/editor.js");
    const { rig, post } = bridged({ installed: ["editor"] });
    for (const name of names) {
      expect(post({ type: "shell.app.open", app: "editor", path: name }), name).toBe(true);
      await flushPromises();
      const launched = editorWindows(rig)[0]!.launch!.path;
      const want = name.startsWith("/") || /^[^/]*:\//.test(name) ? name : `/${name}`;
      expect(launched, "the shell changed the path").toBe(want);
      // What the editor page does with its URL: URLSearchParams, then
      // normalizePath (editor/ui/index.html).
      const url = new URL(frameSrc("editor", "w1", launched), "https://x.example");
      expect(normalizePath(url.searchParams.get("path")), "the editor changed the path").toBe(want);
      // And the message form a running editor gets.
      expect(normalizePath(launched)).toBe(want);
    }
  });

  it("is still refused when it is only spaces", () => {
    const apps = { isKnown: () => true };
    expect(parseAppOpen({ app: "editor", path: "   " }, apps).ok).toBe(false);
  });
});

describe("/kit/shell.js asking the shell", () => {
  afterEach(() => {
    vi.restoreAllMocks();
  });

  /** A kit in a frame whose parent is a real shell's bridge. */
  async function kitInShell(installed: string[]) {
    const { rig } = bridged({ installed });
    const parent = {
      postMessage(data: unknown) {
        rig.shell.frames.handle({ origin: HARNESS_ORIGIN, source: frame, data });
      },
    };
    // The frame's window as the shell sees it: replies posted to it arrive
    // as `message` events from the parent, as the browser delivers them.
    const frame = {
      postMessage(data: unknown) {
        window.dispatchEvent(
          new MessageEvent("message", { data, origin: location.origin, source: parent as never }),
        );
      },
    };
    rig.shell.frames.attach(frame);
    vi.spyOn(window, "parent", "get").mockReturnValue(parent as unknown as Window);
    // @ts-expect-error -- a plain JS module served from public/
    const { shell } = await import("../../public/kit/shell.js");
    return { rig, shell };
  }

  it("is told when the shell refuses to open an app, and why", async () => {
    const { rig, shell } = await kitInShell(["files"]);
    const answer = await shell.openApp("editor", { path: "/a.txt" });
    expect(answer.ok, "the kit said yes to an app that is not installed").toBe(false);
    expect(answer.reason).toMatch(/editor/);
    expect(await shell.openApp("terminal")).toMatchObject({ ok: false });
    expect(rig.shell.windows.all.value).toHaveLength(0);
  });

  it("is told yes when it opened", async () => {
    const { rig, shell } = await kitInShell(["editor"]);
    expect(await shell.openApp("editor", { path: "/a.txt" })).toEqual({ ok: true });
    expect(editorWindows(rig)[0]!.launch?.path).toBe("/a.txt");
  });

  it("can ask whether an app can be opened, without opening it", async () => {
    const { rig, shell } = await kitInShell(["editor"]);
    expect(await shell.canOpen("editor")).toBe(true);
    expect(await shell.canOpen("terminal")).toBe(false);
    expect(await shell.canOpen("nonesuch")).toBe(false);
    expect(rig.shell.windows.all.value).toHaveLength(0);
  });
});
