// @vitest-environment happy-dom
// @vitest-environment-options {"settings":{"navigation":{"disableChildFrameNavigation":true}}}
import { flushPromises } from "@vue/test-utils";
import { describe, expect, it, onTestFinished, vi } from "vitest";
import { nextTick } from "vue";

import AppFramePanel from "../components/panels/AppFramePanel.vue";
import QuickLook from "../components/QuickLook.vue";
import { harness, HARNESS_ORIGIN, mountInShell, type Harness } from "../testing/shell-harness.ts";
import type { ShellWindow } from "../stores/windows.ts";

/**
 * An app's frame asking the shell for Quick Look, which is how Files' Space
 * bar reaches a preview the desktop draws. Real shell, real Quick Look store;
 * the frame is any object the bridge was told to accept.
 */

const ask = (params: unknown, command = "shell.quicklook.open") => ({
  type: "deckjack.shell",
  command,
  params,
});

function bridged() {
  const rig = harness();
  const frame = {};
  const detach = rig.shell.frames.attach(frame);
  const post = (data: unknown, overrides: { origin?: string; source?: unknown } = {}) =>
    rig.shell.frames.handle({
      origin: overrides.origin ?? HARNESS_ORIGIN,
      source: "source" in overrides ? overrides.source : frame,
      data,
    });
  return { rig, frame, detach, post };
}

async function settled(rig: Harness) {
  await flushPromises();
  return rig.shell.quicklook;
}

describe("a frame the shell hosts", () => {
  it("can open Quick Look on a file, with its folder to step through", async () => {
    const { rig, post } = bridged();
    expect(post(ask({ path: "/a.jpg", siblings: ["/a.jpg", "/b.jpg"] }))).toBe(true);
    const ql = await settled(rig);
    expect(ql.visible.value).toBe(true);
    expect(ql.current.value).toBe("/a.jpg");
    expect(ql.position.value).toEqual({ index: 1, total: 2 });
    ql.next();
    expect(ql.current.value).toBe("/b.jpg");
  });

  it("does not get a folder that does not contain the file", async () => {
    // A list without the file would open on one file and step into others.
    const { rig, post } = bridged();
    post(ask({ path: "/a.jpg", siblings: ["/x.jpg", "/y.jpg"] }));
    const ql = await settled(rig);
    expect(ql.current.value).toBe("/a.jpg");
    expect(ql.position.value).toBeNull();
  });

  it("cannot ask for any other shell command", async () => {
    const { rig, post } = bridged();
    rig.connect();
    for (const command of ["shell.window.close", "shell.palette.open", "files.delete"]) {
      expect(post(ask({}, command)), `${command} was dispatched for a frame`).toBe(false);
    }
    await flushPromises();
    expect(rig.shell.ui.paletteOpen.value).toBe(false);
  });
});

describe("a message the shell ignores", () => {
  it("from another origin", async () => {
    const { rig, post } = bridged();
    expect(post(ask({ path: "/a.jpg" }), { origin: "https://elsewhere.example" })).toBe(false);
    expect((await settled(rig)).visible.value).toBe(false);
  });

  it("from a window the shell is not hosting", async () => {
    const { rig, post } = bridged();
    expect(post(ask({ path: "/a.jpg" }), { source: {} })).toBe(false);
    expect(post(ask({ path: "/a.jpg" }), { source: null })).toBe(false);
    expect((await settled(rig)).visible.value).toBe(false);
  });

  it("from a frame whose window has closed", async () => {
    const { rig, post, detach } = bridged();
    detach();
    expect(post(ask({ path: "/a.jpg" }))).toBe(false);
    expect((await settled(rig)).visible.value).toBe(false);
  });

  it("that is not one of ours", () => {
    const { post } = bridged();
    expect(post({ type: "something-else", command: "shell.quicklook.open" })).toBe(false);
    expect(post("shell.quicklook.open")).toBe(false);
    expect(post(null)).toBe(false);
  });
});

describe("an app window", () => {
  it("lets its own frame ask once it has loaded", async () => {
    vi.stubGlobal("fetch", vi.fn(async () => new Response("<!doctype html>", { status: 200 })));
    onTestFinished(() => void vi.unstubAllGlobals());
    const rig = harness();
    const window = {
      id: "w1",
      app: "files",
      appId: "files",
      title: "Files",
    } as unknown as ShellWindow;
    const { wrapper } = mountInShell(AppFramePanel, {
      harness: rig,
      props: { window },
      attachTo: document.body,
    });
    const iframe = wrapper.find("iframe").element as HTMLIFrameElement;
    const source = iframe.contentWindow;
    expect(source, "happy-dom gave the frame no window").toBeTruthy();

    const message = { origin: HARNESS_ORIGIN, source, data: ask({ path: "/a.jpg" }) };
    expect(rig.shell.frames.handle(message), "accepted before the frame loaded").toBe(false);

    // The blank document a frame has before it is pointed at the app.
    iframe.dispatchEvent(new Event("load"));
    expect(rig.shell.frames.handle(message), "accepted from a blank frame").toBe(false);

    await flushPromises();
    iframe.dispatchEvent(new Event("load"));
    expect(rig.shell.frames.handle(message)).toBe(true);

    wrapper.unmount();
    expect(rig.shell.frames.handle(message), "still accepted after the window closed").toBe(false);
  });
});

describe("the preview, opened from a frame", () => {
  it("takes focus, so arrows and Escape work, and gives it back when it closes", async () => {
    const rig = harness();
    const before = document.createElement("button");
    document.body.appendChild(before);
    before.focus();

    const { wrapper } = mountInShell(QuickLook, { harness: rig, attachTo: document.body });
    rig.shell.quicklook.open("/a.jpg", ["/a.jpg", "/b.jpg"]);
    await nextTick();
    await nextTick();
    const dialog = wrapper.find("[role=dialog]").element as HTMLElement;
    expect(document.activeElement, "focus stayed where it was").toBe(dialog);

    dialog.dispatchEvent(new KeyboardEvent("keydown", { key: "ArrowRight", bubbles: true }));
    expect(rig.shell.quicklook.current.value).toBe("/b.jpg");

    dialog.dispatchEvent(new KeyboardEvent("keydown", { key: " ", bubbles: true }));
    await nextTick();
    expect(rig.shell.quicklook.visible.value, "Space did not close the peek").toBe(false);
    expect(document.activeElement, "focus was not given back").toBe(before);
    wrapper.unmount();
    before.remove();
  });

  it("does not close on Space pressed on a control inside it", async () => {
    // On a focused <video> Space is play and pause, not "close".
    const rig = harness();
    const { wrapper } = mountInShell(QuickLook, { harness: rig, attachTo: document.body });
    rig.shell.quicklook.open("/a.jpg");
    await nextTick();
    const close = wrapper.find("button").element as HTMLElement;
    close.dispatchEvent(new KeyboardEvent("keydown", { key: " ", bubbles: true }));
    expect(rig.shell.quicklook.visible.value).toBe(true);
    wrapper.unmount();
  });
});
