// @vitest-environment happy-dom
import { flushPromises } from "@vue/test-utils";
import { describe, expect, it } from "vitest";

import { AppFramePanel, panelFor } from "../components/panels/index.ts";
import TopBar from "../components/TopBar.vue";
import { harness, mountInShell, type Harness } from "../testing/shell-harness.ts";
import { openWhenReady } from "./shell-commands.ts";

/**
 * The shell and the node's app list, end to end: real stores, a fake socket,
 * and `system.apps` answered over the wire the way a core answers it.
 */

const photo = {
  name: "photo",
  title: "Photos",
  version: "2.0.0",
  lifecycle: "on-demand",
  status: "live",
  tag: "Ph",
  summary: "Edit photographs",
  min_width: 360,
  default_width: 900,
  default_height: 600,
  compact: true,
  wide: true,
};

function invokes(rig: Harness, command: string) {
  return rig
    .socket()
    .frames()
    .filter((f) => f.type === "invoke" && f.command === command);
}

/** Answer the newest outstanding call to `command` with `value`. */
async function answer(rig: Harness, command: string, value: unknown) {
  const call = invokes(rig, command).at(-1);
  expect(call, `the shell never asked for ${command}`).toBeTruthy();
  rig.socket().deliver({ type: "response", id: call!.id, outcome: { status: "ok", value } });
  await flushPromises();
}

async function withPhotoInstalled(): Promise<Harness> {
  const rig = harness();
  rig.connect();
  await answer(rig, "system.apps", { apps: [photo] });
  return rig;
}

describe("the installed-app list", () => {
  it("is asked for when the node says hello, not while the socket is still connecting", async () => {
    const rig = harness();
    // Built, not connected: a call now would be refused by the bus, which is
    // how the list used to come back empty on every page load.
    rig.shell.bus.connect();
    expect(invokes(rig, "system.apps")).toHaveLength(0);

    rig.socket().open();
    expect(invokes(rig, "system.apps")).toHaveLength(1);
    await answer(rig, "system.apps", { apps: [photo] });
    expect(rig.shell.apps.isInstalled("photo")).toBe(true);
  });

  it("is asked for again when a node says hello again", async () => {
    // A reconnect, or a different node taking over, may have different apps.
    const rig = await withPhotoInstalled();
    rig.socket().deliver({
      type: "welcome",
      protocol: 1,
      connection: 2,
      node_id: "n",
      host_name: "other",
    });
    await answer(rig, "system.apps", { apps: [] });
    expect(rig.shell.apps.isInstalled("photo")).toBe(false);
  });
});

describe("an app installed on the node", () => {
  it("is in the dock, with its own tag", async () => {
    const rig = await withPhotoInstalled();
    const { wrapper } = mountInShell(TopBar, { harness: rig });
    await flushPromises();
    const button = wrapper.findAll("button").find((b) => b.text().includes("Photos"));
    expect(button, "the dock has no button for an installed app").toBeTruthy();
    expect(button!.text()).toContain("Ph");
  });

  it("can be opened from the palette", async () => {
    const rig = await withPhotoInstalled();
    const entry = rig.shell.commands.byId("app.open.photo");
    expect(entry, "the palette has no entry for an installed app").toBeTruthy();
    expect(entry!.title).toBe("Open Photos");
  });

  it("opens at its manifest's size and shows its own UI", async () => {
    const rig = await withPhotoInstalled();
    void rig.shell.commands.dispatch("app.open.photo");
    await flushPromises();

    const opened = invokes(rig, "shell.window.open").at(-1);
    expect(opened, "opening an installed app recorded nothing on the node").toBeTruthy();
    const params = opened!.params as Record<string, unknown>;
    expect(params.app).toBe("photo");
    expect(params.w).toBe(900);
    expect(params.h).toBe(600);

    const window = rig.shell.windows.all.value.find((w) => w.app === "photo")!;
    expect(panelFor(window.appId, rig.shell.apps.isInstalled)).toBe(AppFramePanel);
  });

  it("leaves the palette when it is uninstalled", async () => {
    const rig = await withPhotoInstalled();
    rig.socket().deliver({ type: "welcome", protocol: 1, connection: 2, node_id: "n", host_name: "p" });
    await answer(rig, "system.apps", { apps: [] });
    expect(rig.shell.commands.byId("app.open.photo")).toBeNull();
    // The built-in entries stay registered through the swap.
    expect(rig.shell.commands.byId("app.open.terminal")).toBeTruthy();
  });
});

describe("a launch shortcut", () => {
  it("waits for the node rather than failing while the socket connects", async () => {
    const rig = harness();
    openWhenReady(rig.shell, "terminal");
    await flushPromises();
    expect(rig.shell.windows.all.value, "opened with no node to record it").toHaveLength(0);

    rig.connect();
    await flushPromises();
    const opened = invokes(rig, "shell.window.open");
    expect(opened.map((f) => (f.params as Record<string, unknown>).app)).toEqual(["terminal"]);
  });

  it("waits for an installed app to be known, and opens it once", async () => {
    const rig = harness();
    openWhenReady(rig.shell, "photo");
    rig.connect();
    await flushPromises();
    expect(invokes(rig, "shell.window.open"), "opened before the app was known").toHaveLength(0);

    await answer(rig, "system.apps", { apps: [photo] });
    // The node drops and comes back. The condition the shortcut waits on goes
    // false and true again; a shortcut is one launch, not a standing order.
    const first = rig.socket();
    first.close();
    await flushPromises();
    expect(rig.shell.bus.status.value).not.toBe("open");
    rig.advance(60_000);
    expect(rig.sockets.length, "the bus never reconnected").toBeGreaterThan(1);
    rig.socket().open();
    await flushPromises();
    await answer(rig, "system.apps", { apps: [photo] });
    const opened = [first, rig.socket()]
      .flatMap((s) => s.frames())
      .filter((f) => f.type === "invoke" && f.command === "shell.window.open");
    expect(opened.map((f) => (f.params as Record<string, unknown>).app)).toEqual(["photo"]);
  });

  it("never opens an app nothing knows", async () => {
    const rig = harness();
    openWhenReady(rig.shell, "nope");
    rig.connect();
    await answer(rig, "system.apps", { apps: [photo] });
    expect(invokes(rig, "shell.window.open")).toHaveLength(0);
  });
});

describe("an app awaiting the operator's approval", () => {
  // After the approval fingerprint moved to SHA-256, every approved app went
  // stale, and nothing in the shell could approve it again: the window failed
  // to open, and the advice pointed at a CLI command that does not exist.
  const notes = {
    name: "notes",
    title: "Notes",
    version: "1.0.0",
    lifecycle: "on-demand",
    status: "unavailable",
    compact: true,
    wide: true,
    approval: "stale",
    asks: "read and write Notes",
  };

  it("can be approved from the palette, which says what it asks for", async () => {
    const rig = harness();
    rig.connect();
    await answer(rig, "system.apps", { apps: [notes, photo] });

    const entry = rig.shell.commands.byId("app.approve.notes");
    expect(entry, "the palette has no way to approve a stale app").toBeTruthy();
    expect(entry!.title).toBe("Approve Notes");
    expect(entry!.summary).toContain("read and write Notes");
    // An app whose approval stands, or that needs none, offers nothing.
    expect(rig.shell.commands.byId("app.approve.photo")).toBeNull();

    void rig.shell.commands.dispatch("app.approve.notes");
    await flushPromises();
    const approve = invokes(rig, "system.app.approve").at(-1);
    expect(approve, "approving sent nothing to the node").toBeTruthy();
    expect(approve!.params).toEqual({ app: "notes" });

    // Once the node has recorded it, the list is asked for again and the
    // entry goes.
    const before = invokes(rig, "system.apps").length;
    await answer(rig, "system.app.approve", { app: "notes" });
    expect(invokes(rig, "system.apps").length).toBe(before + 1);
    await answer(rig, "system.apps", { apps: [{ ...notes, approval: "approved" }, photo] });
    expect(rig.shell.commands.byId("app.approve.notes")).toBeNull();
  });

  it("is offered for a new app that has never been approved too", async () => {
    const rig = harness();
    rig.connect();
    await answer(rig, "system.apps", { apps: [{ ...notes, approval: "required" }] });
    expect(rig.shell.commands.byId("app.approve.notes")).toBeTruthy();
  });
});
