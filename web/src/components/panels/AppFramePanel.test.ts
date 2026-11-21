// @vitest-environment happy-dom
// @vitest-environment-options {"settings":{"navigation":{"disableChildFrameNavigation":true}}}
import { flushPromises } from "@vue/test-utils";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { nextTick, reactive } from "vue";

import { harness, mountInShell, type Harness } from "../../testing/shell-harness.ts";
import { APP_LAUNCH_MESSAGE } from "../../shell/open-app.ts";
import type { ShellWindow } from "../../stores/windows.ts";
import AppFramePanel from "./AppFramePanel.vue";

/**
 * An installed app's frame, against the core's real behavior while an
 * on-demand app starts: the window record lands, the app is still spawning,
 * and `/apps/<name>/` answers `503` with `Retry-After` until it is live.
 *
 * The node is played by a stubbed `fetch` (what the core answers for the
 * app's page right now). The frame itself never navigates under happy-dom,
 * so what is asserted is what the browser would be told to load, and when.
 */

/** What `/apps/<name>/` answers, in order; the last answer repeats. */
let answers: Array<{ status: number; retryAfter?: string }> = [];
let asked: string[] = [];
/** Whether the node has recorded the window (`shell.window.open` answered). */
let recorded = true;

beforeEach(() => {
  answers = [{ status: 200 }];
  asked = [];
  recorded = true;
  vi.stubGlobal(
    "fetch",
    vi.fn(async (url: string) => {
      asked.push(String(url));
      // Before the node has recorded the window it has not started the app,
      // and the app's page is not found.
      if (!recorded) return new Response("no such app asset", { status: 404 });
      const next = answers.length > 1 ? answers.shift()! : answers[0]!;
      const headers = new Headers();
      if (next.retryAfter !== undefined) headers.set("retry-after", next.retryAfter);
      return new Response(next.status === 200 ? "<!doctype html>" : "restarting", {
        status: next.status,
        headers,
      });
    }),
  );
});

afterEach(() => {
  vi.useRealTimers();
  vi.unstubAllGlobals();
  vi.restoreAllMocks();
});

function editorWindow(extra: Partial<ShellWindow> = {}) {
  return reactive({
    id: "w7",
    app: "editor",
    appId: "editor",
    title: "Editor",
    loading: false,
    launch: null,
    ...extra,
  }) as unknown as ShellWindow;
}

function mounted(w: ShellWindow, rig: Harness = harness()) {
  const { wrapper } = mountInShell(AppFramePanel, {
    harness: rig,
    props: { window: w },
    attachTo: document.body,
  });
  const iframe = () => wrapper.find("iframe").element as HTMLIFrameElement;
  const url = () => {
    const src = iframe().getAttribute("src");
    return src ? new URL(src, "https://x.example") : null;
  };
  const sent: unknown[] = [];
  const spy = () =>
    vi.spyOn(iframe().contentWindow!, "postMessage").mockImplementation(((data: unknown) => {
      sent.push(data);
    }) as typeof window.postMessage);
  return { wrapper, rig, iframe, url, sent, spy };
}

describe("a window on an app that is still starting", () => {
  it("loads the app once it is live, not the 'restarting' page", async () => {
    vi.useFakeTimers({ toFake: ["setTimeout", "clearTimeout"] });
    // The window is created before the node has recorded it (and it stays
    // `loading`: the node sends no `windows` event to clear it), and the
    // node then answers 503 until the app process is up.
    answers = [{ status: 503, retryAfter: "1" }, { status: 503, retryAfter: "1" }, { status: 200 }];
    recorded = false;
    const w = editorWindow({
      loading: true,
      launch: { path: "/src/a.txt", seq: 1 },
    });
    const f = mounted(w);
    await flushPromises();
    expect(asked.length).toBeGreaterThan(0);
    expect(f.url(), "the frame loaded the 404 from before the node recorded the window").toBeNull();
    await vi.advanceTimersByTimeAsync(250);
    expect(f.url(), "a 404 was believed at once").toBeNull();

    recorded = true;
    await vi.advanceTimersByTimeAsync(250);
    expect(f.url(), "the frame loaded the 503 page").toBeNull();

    await vi.advanceTimersByTimeAsync(1000);
    expect(f.url(), "still the 503 page after one retry").toBeNull();

    await vi.advanceTimersByTimeAsync(1000);
    const url = f.url();
    expect(url, "the frame never loaded once the app was live").not.toBeNull();
    expect(url!.pathname).toBe("/apps/editor/");
    expect(url!.searchParams.get("path")).toBe("/src/a.txt");
    expect(url!.searchParams.get("window")).toBe("w7");
    f.wrapper.unmount();
  });

  it("hands over every file asked for while it started, once the real page has loaded", async () => {
    vi.useFakeTimers({ toFake: ["setTimeout", "clearTimeout"] });
    answers = [{ status: 503, retryAfter: "1" }, { status: 200 }];
    const w = editorWindow({
      loading: true,
      launch: { path: "/a.txt", seq: 1 },
    });
    const f = mounted(w);
    f.spy();
    // A blank frame's own load (about:blank) is not the app.
    f.iframe().dispatchEvent(new Event("load"));
    w.launch = { path: "/b.txt", seq: 2 };
    await nextTick();
    await flushPromises();
    w.launch = { path: "/c.txt", seq: 3 };
    await nextTick();
    expect(f.sent, "posted into a page that was not the app").toEqual([]);

    await vi.advanceTimersByTimeAsync(1000);
    expect(f.url()!.searchParams.get("path")).toBe("/a.txt");
    f.iframe().dispatchEvent(new Event("load"));
    expect(f.sent).toEqual([
      { type: APP_LAUNCH_MESSAGE, path: "/b.txt" },
      { type: APP_LAUNCH_MESSAGE, path: "/c.txt" },
    ]);
    f.wrapper.unmount();
  });

  it("shows an app that cannot run as it is, rather than waiting forever", async () => {
    vi.useFakeTimers({ toFake: ["setTimeout", "clearTimeout"] });
    recorded = false;
    const f = mounted(editorWindow({ loading: true }));
    await flushPromises();
    expect(f.url()).toBeNull();
    await vi.advanceTimersByTimeAsync(5000);
    expect(f.url(), "the window never showed the core's answer").not.toBeNull();
    f.wrapper.unmount();
  });

  it("does not wait for an app that is already live", async () => {
    const f = mounted(editorWindow({ launch: { path: "/a.txt", seq: 1 } }));
    await flushPromises();
    expect(f.url()!.searchParams.get("path")).toBe("/a.txt");
    f.wrapper.unmount();
  });
});

describe("the compact shell's one panel, switched between windows", () => {
  it("does not reopen the file a window was first opened on every time it comes back", async () => {
    const editor = editorWindow({ launch: { path: "/src/a.txt", seq: 1 } });
    const files = reactive({
      id: "w1",
      app: "files",
      appId: "files",
      title: "Files",
      loading: false,
    }) as unknown as ShellWindow;
    const f = mounted(editor);
    await flushPromises();
    expect(f.url()!.searchParams.get("path")).toBe("/src/a.txt");
    f.iframe().dispatchEvent(new Event("load"));

    await f.wrapper.setProps({ window: files });
    await flushPromises();
    await f.wrapper.setProps({ window: editor });
    await flushPromises();
    const back = f.url()!;
    expect(back.pathname).toBe("/apps/editor/");
    expect(back.searchParams.get("path"), "the delivered launch was replayed").toBeNull();
    f.wrapper.unmount();
  });

  it("still delivers a file asked for while the window was not shown", async () => {
    const editor = editorWindow({ launch: { path: "/a.txt", seq: 1 } });
    const files = reactive({
      id: "w1",
      app: "files",
      appId: "files",
      title: "Files",
      loading: false,
    }) as unknown as ShellWindow;
    const f = mounted(editor);
    await flushPromises();
    f.iframe().dispatchEvent(new Event("load"));
    await f.wrapper.setProps({ window: files });
    await flushPromises();
    editor.launch = { path: "/b.txt", seq: 2 };
    await f.wrapper.setProps({ window: editor });
    await flushPromises();
    expect(f.url()!.searchParams.get("path")).toBe("/b.txt");
    f.wrapper.unmount();
  });

  it("survives the panel being remounted, as switching layouts does", async () => {
    const editor = editorWindow({ launch: { path: "/a.txt", seq: 1 } });
    const rig = harness();
    const first = mounted(editor, rig);
    await flushPromises();
    first.iframe().dispatchEvent(new Event("load"));
    first.wrapper.unmount();
    const second = mounted(editor, rig);
    await flushPromises();
    expect(second.url()!.searchParams.get("path")).toBeNull();
    second.wrapper.unmount();
  });
});

describe("the frame's own document", () => {
  it("gets the desktop's density and theme, so --dj-touch and the colors resolve", async () => {
    const rig = harness();
    rig.shell.theme.setDensity("cozy");
    const f = mounted(editorWindow(), rig);
    await flushPromises();
    f.iframe().dispatchEvent(new Event("load"));
    const doc = f.iframe().contentDocument!;
    expect(doc.documentElement.dataset.density).toBe("cozy");
    expect(doc.documentElement.dataset.theme).toBe(rig.shell.theme.theme.value);
    // Which of the stylesheet's own rules setting --dj-touch apply to the
    // frame's root, and what they set it to. (happy-dom's computed style
    // does not honor attribute values in selectors, so the match is asked
    // of the selector engine directly.)
    const touch = () => {
      const css = doc.getElementById("dj-tokens")!.textContent!;
      return [...css.matchAll(/([^{}]+)\{([^}]*)\}/g)]
        .filter(([, , body]) => /--dj-touch:/.test(body!))
        .filter(([, selector]) => doc.documentElement.matches(selector!.trim()))
        .map(([, , body]) => /--dj-touch:\s*([^;]+);/.exec(body!)![1]!.trim());
    };
    expect(touch(), "no rule setting --dj-touch applies in the frame").toEqual(["44px"]);

    rig.shell.theme.setDensity("comfortable");
    await nextTick();
    expect(doc.documentElement.dataset.density).toBe("comfortable");
    expect(touch()).toEqual(["48px"]);
    f.wrapper.unmount();
  });

  it("raises touch targets to at least 44px on a coarse pointer, at every density", async () => {
    const rig = harness();
    const f = mounted(editorWindow(), rig);
    await flushPromises();
    f.iframe().dispatchEvent(new Event("load"));
    const doc = f.iframe().contentDocument!;
    // compact is 40px/30px, cozy 44px/38px, comfortable 48px/46px without
    // a coarse pointer. A coarse pointer raises each to 44px and never
    // lowers one.
    const expected = {
      compact: { touch: 44, row: 44 },
      cozy: { touch: 44, row: 44 },
      comfortable: { touch: 48, row: 46 },
    } as const;
    for (const [density, want] of Object.entries(expected)) {
      rig.shell.theme.setDensity(density as keyof typeof expected);
      await nextTick();
      expect(doc.documentElement.dataset.density).toBe(density);
      const style = cascade(doc, { coarse: true });
      expect(px(style, "--dj-touch"), `--dj-touch at ${density}`).toBe(want.touch);
      expect(px(style, "--dj-row"), `--dj-row at ${density}`).toBe(want.row);
      // And without a coarse pointer the density's own values stand.
      const fine = cascade(doc, { coarse: false });
      expect(px(fine, "--dj-touch")).toBe(
        { compact: 40, cozy: 44, comfortable: 48 }[density as keyof typeof expected],
      );
    }
    f.wrapper.unmount();
  });
});

/**
 * The custom properties the frame's root ends up with, from the frame's own
 * `<style>` elements: rules whose selector matches `<html>` (and whose
 * `@media` matches the given pointer), ranked by specificity and then source
 * order, with `var()` references substituted and a reference cycle making
 * the property invalid, as CSS does. happy-dom's computed style honors
 * neither attribute selectors' specificity nor custom-property cycles, so
 * the cascade is resolved here, against the selector engine's matches.
 */
function cascade(doc: Document, media: { coarse: boolean }): Map<string, string | null> {
  type Decl = { value: string; spec: number; order: number };
  const won = new Map<string, Decl>();
  let order = 0;
  const css = [...doc.querySelectorAll("style")].map((s) => s.textContent ?? "").join("\n");
  const visit = (text: string, active: boolean) => {
    let i = 0;
    while (i < text.length) {
      const open = text.indexOf("{", i);
      if (open < 0) break;
      const head = text.slice(i, open).trim();
      // Find the matching close brace.
      let depth = 1;
      let j = open + 1;
      while (j < text.length && depth > 0) {
        if (text[j] === "{") depth++;
        else if (text[j] === "}") depth--;
        j++;
      }
      const body = text.slice(open + 1, j - 1);
      i = j;
      if (head.startsWith("@media")) {
        const coarse = /\(pointer:\s*coarse\)/.test(head);
        visit(body, active && (!coarse || media.coarse));
        continue;
      }
      if (head.startsWith("@") || !active) continue;
      for (const selector of head.split(",").map((s) => s.trim())) {
        let matches = false;
        try {
          matches = doc.documentElement.matches(selector);
        } catch {
          matches = false;
        }
        if (!matches) continue;
        // ids, then classes/attributes/pseudo-classes, then types.
        const a = (selector.match(/#[\w-]+/g) ?? []).length;
        const b = (selector.match(/\.[\w-]+|\[[^\]]*\]|:(?!:)[\w-]+/g) ?? []).length;
        const c = (selector.replace(/\[[^\]]*\]/g, "").match(/(^|[\s>+~])[a-z][\w-]*/gi) ?? [])
          .length;
        const spec = a * 10000 + b * 100 + c;
        for (const m of body.matchAll(/(--[\w-]+)\s*:\s*([^;]+)/g)) {
          const prior = won.get(m[1]!);
          const decl = { value: m[2]!.trim(), spec, order: order++ };
          if (!prior || decl.spec >= prior.spec) won.set(m[1]!, decl);
        }
      }
    }
  };
  visit(css, true);
  const resolved = new Map<string, string | null>();
  const resolve = (name: string, seen: Set<string>): string | null => {
    if (resolved.has(name)) return resolved.get(name)!;
    if (seen.has(name)) return null;
    const decl = won.get(name);
    if (!decl) return null;
    seen.add(name);
    let invalid = false;
    const value = decl.value.replace(/var\((--[\w-]+)\)/g, (_, ref: string) => {
      const v = resolve(ref, seen);
      if (v === null) invalid = true;
      return v ?? "";
    });
    seen.delete(name);
    const out = invalid ? null : value;
    resolved.set(name, out);
    return out;
  };
  for (const name of won.keys()) resolve(name, new Set());
  return resolved;
}

/** A resolved length in px, evaluating a `max()` of px lengths. */
function px(style: Map<string, string | null>, name: string): number | null {
  const value = style.get(name);
  if (value == null) return null;
  const max = /^max\((.*)\)$/.exec(value);
  const parts = (max ? max[1]!.split(",") : [value]).map((p) => p.trim());
  if (!parts.every((p) => /^\d+(\.\d+)?px$/.test(p))) return null;
  return Math.max(...parts.map((p) => parseFloat(p)));
}
