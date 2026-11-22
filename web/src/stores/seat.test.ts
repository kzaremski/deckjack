// @vitest-environment happy-dom
import { flushPromises } from "@vue/test-utils";
import { afterEach, describe, expect, it } from "vitest";

import { harness, memoryStorage, type Harness } from "../testing/shell-harness.ts";
import type { SeatStatus, WireWindow } from "../protocol.ts";
import {
  cleanLabel,
  clock,
  createDeviceLabel,
  defaultDeviceLabel,
  DEVICE_KEY,
  DEVICE_MAX,
  refineFromClientHints,
} from "./seat.ts";

/**
 * The single seat on this side: the real bus, the real seat store and the
 * real window store over a fake socket, driven by the frames a node sends.
 */

let rigs: Harness[] = [];

afterEach(() => {
  for (const rig of rigs) rig.dispose();
  rigs = [];
});

function rig(options: Parameters<typeof harness>[0] = {}): Harness {
  const r = harness(options);
  rigs.push(r);
  return r;
}

function invokes(r: Harness, command: string) {
  return r
    .socket()
    .frames()
    .filter((f) => f.type === "invoke" && f.command === command);
}

async function answer(r: Harness, command: string, outcome: unknown) {
  const call = invokes(r, command).at(-1);
  expect(call, `the shell never asked for ${command}`).toBeTruthy();
  r.socket().deliver({ type: "response", id: call!.id, outcome, elapsed_ms: 1 });
  await flushPromises();
}

const SINCE = new Date(2026, 10, 21, 9, 5).getTime();

function seat(r: Harness, status: SeatStatus) {
  r.socket().deliver({ type: "event", topic: "seat", payload: status, at_ms: 0 });
}

const held: SeatStatus = {
  held: true,
  claimable: false,
  holder: { device: "Firefox on Linux", since_ms: SINCE, connected: true },
};

const elsewhere: SeatStatus = {
  held: false,
  claimable: false,
  holder: { device: "Chrome on Pixel", since_ms: SINCE, connected: true },
};

const vacant: SeatStatus = { held: false, claimable: true, holder: null };

function wire(id: string, x = 10): WireWindow {
  return {
    id,
    app: "notes",
    title: "Notes",
    minWidth: 320,
    minHeight: 0,
    desktop: { x, y: 20, w: 600, h: 400, z: 0, minimized: false, maximized: false },
  };
}

describe("the device label", () => {
  it("defaults to the browser and the device", () => {
    const pixel =
      "Mozilla/5.0 (Linux; Android 14; Pixel 7) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/120.0 Mobile Safari/537.36";
    expect(defaultDeviceLabel(pixel)).toBe("Chrome on Pixel");
    expect(defaultDeviceLabel("Mozilla/5.0 (X11; Linux x86_64; rv:140.0) Gecko/20100101 Firefox/140.0")).toBe(
      "Firefox on Linux",
    );
    expect(
      defaultDeviceLabel(
        "Mozilla/5.0 (iPhone; CPU iPhone OS 18_0 like Mac OS X) AppleWebKit/605.1.15 (KHTML, like Gecko) Version/18.0 Mobile/15E148 Safari/604.1",
      ),
    ).toBe("Safari on iPhone");
    expect(
      defaultDeviceLabel(
        "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/120.0 Safari/537.36 Edg/120.0",
      ),
    ).toBe("Edge on Windows");
    expect(
      defaultDeviceLabel(
        "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/605.1.15 (KHTML, like Gecko) Version/18.0 Safari/605.1.15",
      ),
    ).toBe("Safari on Mac");
  });

  it("takes the model from client hints, since Chrome on Android reports none", () => {
    // The reduced user agent every Chrome on Android has sent since 110.
    const reduced =
      "Mozilla/5.0 (Linux; Android 10; K) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/131.0.0.0 Mobile Safari/537.36";
    expect(defaultDeviceLabel(reduced)).toBe("Chrome on Android");
    expect(defaultDeviceLabel(reduced, { model: "Pixel 8" })).toBe("Chrome on Pixel 8");
    expect(defaultDeviceLabel(reduced, { model: "  " })).toBe("Chrome on Android");
  });

  it("calls a Mac with a touch screen an iPad, since iPadOS Safari reports a Mac", () => {
    const mac =
      "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/605.1.15 (KHTML, like Gecko) Version/18.0 Safari/605.1.15";
    expect(defaultDeviceLabel(mac, { touchPoints: 5 })).toBe("Safari on iPad");
    expect(defaultDeviceLabel(mac, { touchPoints: 0 })).toBe("Safari on Mac");
  });

  it("refines the default when the model arrives, unless the operator named the device", async () => {
    const reduced = "Mozilla/5.0 (Linux; Android 10; K) Chrome/131.0.0.0 Mobile Safari/537.36";
    const nav = (model: string) =>
      ({ userAgentData: { getHighEntropyValues: async () => ({ model }) } }) as unknown as Navigator;

    const fresh = createDeviceLabel(memoryStorage(), reduced);
    expect(fresh.label.value).toBe("Chrome on Android");
    refineFromClientHints(fresh, nav("Pixel 8"));
    await flushPromises();
    expect(fresh.label.value).toBe("Chrome on Pixel 8");
    // The refined default is what an empty rename goes back to.
    fresh.rename("");
    expect(fresh.label.value).toBe("Chrome on Pixel 8");

    const storage = memoryStorage();
    storage.write(DEVICE_KEY, "Kitchen tablet");
    const named = createDeviceLabel(storage, reduced);
    refineFromClientHints(named, nav("SM-X710"));
    await flushPromises();
    expect(named.label.value).toBe("Kitchen tablet");

    // A browser without client hints changes nothing.
    const plain = createDeviceLabel(memoryStorage(), reduced);
    refineFromClientHints(plain, {} as Navigator);
    await flushPromises();
    expect(plain.label.value).toBe("Chrome on Android");
  });

  it("is cleaned the way the node cleans it", () => {
    expect(cleanLabel("  my ‮phone\u0007  ")).toBe("my phone");
    expect(cleanLabel("x".repeat(DEVICE_MAX + 5))).toHaveLength(DEVICE_MAX);
  });

  it("goes out with every hello, and a rename is kept for the next connection", async () => {
    const storage = memoryStorage();
    const first = rig({
      storage,
      userAgent: "Mozilla/5.0 (Linux; Android 14; Pixel 7) Chrome/120.0 Mobile Safari/537.36",
    });
    first.connect();
    expect(first.socket().frames()[0]).toMatchObject({ type: "hello", device: "Chrome on Pixel" });

    await first.shell.seat.rename("  Kitchen   tablet ");
    expect(storage.read(DEVICE_KEY)).toBe("Kitchen tablet");

    // A reload: a new shell over the same storage.
    const second = rig({ storage, userAgent: "Mozilla/5.0 (Linux; Android 14; Pixel 7) Chrome/120.0" });
    second.connect();
    expect(second.socket().frames()[0]).toMatchObject({ type: "hello", device: "Kitchen tablet" });

    // An empty name goes back to the default.
    await second.shell.seat.rename("   ");
    expect(second.shell.seat.device.label.value).toBe("Chrome on Pixel");
  });

  it("relabels a seated device for the others at once", async () => {
    const r = rig();
    r.connect();
    seat(r, held);
    await flushPromises();
    void r.shell.seat.rename("Desk");
    expect(invokes(r, "seat.take").at(-1)?.params).toEqual({ device: "Desk" });
  });

  it("formats the holder's time as HH:MM", () => {
    expect(clock(SINCE)).toBe("09:05");
  });
});

describe("the phases", () => {
  it("is unknown until the node says, and the workspace shows meanwhile", () => {
    const r = rig();
    r.connect();
    expect(r.shell.seat.phase.value).toBe("unknown");
    expect(r.shell.seat.showWorkspace.value).toBe(true);
  });

  it("is held when this connection has the seat", async () => {
    const r = rig();
    r.connect();
    seat(r, held);
    expect(r.shell.seat.phase.value).toBe("held");
    expect(r.shell.seat.showWorkspace.value).toBe(true);
  });

  it("is in use, not taken, when this connection never had it", async () => {
    const r = rig();
    r.connect();
    seat(r, elsewhere);
    expect(r.shell.seat.phase.value).toBe("in-use");
    expect(r.shell.seat.showWorkspace.value).toBe(false);
  });

  it("is taken when another device takes it from this connection", async () => {
    const r = rig();
    r.connect();
    seat(r, held);
    seat(r, elsewhere);
    expect(r.shell.seat.phase.value).toBe("taken");
    expect(r.shell.seat.showWorkspace.value).toBe(false);
  });

  it("forgets what an old connection held", async () => {
    const r = rig();
    r.connect();
    seat(r, held);
    // A new connection says hello: whatever was held was held on the last one.
    r.socket().deliver({ type: "welcome", protocol: 1, connection: 2, node_id: "n", host_name: "pocket" });
    expect(r.shell.seat.phase.value).toBe("unknown");
    seat(r, elsewhere);
    expect(r.shell.seat.phase.value).toBe("taken");
  });

  it("keeps the takeover screen, and its words, across a reconnect", async () => {
    const r = rig();
    r.connect();
    seat(r, held);
    seat(r, elsewhere);
    expect(r.shell.seat.phase.value).toBe("taken");
    // The socket blips; between the welcome and the seat event the
    // workspace must not mount.
    r.socket().deliver({ type: "welcome", protocol: 1, connection: 2, node_id: "n", host_name: "pocket" });
    expect(r.shell.seat.showWorkspace.value).toBe(false);
    expect(r.shell.seat.phase.value).toBe("taken");
    seat(r, elsewhere);
    expect(r.shell.seat.phase.value).toBe("taken");

    const never = rig();
    never.connect();
    seat(never, elsewhere);
    never.socket().deliver({ type: "welcome", protocol: 1, connection: 2, node_id: "n", host_name: "pocket" });
    expect(never.shell.seat.phase.value).toBe("in-use");
  });
});

describe("a free seat", () => {
  it("is claimed without asking, once, and never taken", async () => {
    const r = rig();
    r.connect();
    seat(r, vacant);
    seat(r, vacant);
    expect(r.shell.seat.phase.value).toBe("claiming");
    expect(r.shell.seat.showWorkspace.value).toBe(true);
    expect(invokes(r, "seat.claim")).toHaveLength(1);
    expect(invokes(r, "seat.take")).toHaveLength(0);
    expect(r.shell.ui.confirmation.value).toBeNull();

    await answer(r, "seat.claim", { status: "ok", value: held });
    expect(r.shell.seat.phase.value).toBe("held");
  });
});

describe("taking over", () => {
  it("asks first, and declining sends nothing", async () => {
    const r = rig();
    r.connect();
    seat(r, elsewhere);
    const took = r.shell.seat.takeOver();
    await flushPromises();
    const asked = r.shell.ui.confirmation.value;
    expect(asked?.action).toBe("Take over");
    expect(asked?.detail).toContain("Chrome on Pixel");
    asked!.resolve(false);
    expect(await took).toBe(false);
    expect(invokes(r, "seat.take")).toHaveLength(0);
  });

  it("takes the seat with this device's name once the operator agrees", async () => {
    const r = rig();
    r.connect();
    seat(r, elsewhere);
    const took = r.shell.seat.takeOver();
    await flushPromises();
    r.shell.ui.confirmation.value!.resolve(true);
    await flushPromises();
    expect(invokes(r, "seat.take").at(-1)?.params).toEqual({ device: "Firefox on Linux" });
    await answer(r, "seat.take", { status: "ok", value: held });
    expect(await took).toBe(true);
    expect(r.shell.seat.phase.value).toBe("held");
  });

  it("says take back when the seat was taken from this device", async () => {
    const r = rig();
    r.connect();
    seat(r, held);
    seat(r, elsewhere);
    void r.shell.seat.takeOver();
    await flushPromises();
    expect(r.shell.ui.confirmation.value?.action).toBe("Take back");
  });

  it("says why when the node does not hand it over", async () => {
    const r = rig();
    r.connect();
    seat(r, elsewhere);
    const took = r.shell.seat.takeOver();
    await flushPromises();
    r.shell.ui.confirmation.value!.resolve(true);
    await flushPromises();
    await answer(r, "seat.take", { status: "error", error: { code: "failed", message: "no handshake" } });
    expect(await took).toBe(false);
    expect(r.shell.seat.lastError.value).toBe("no handshake");
  });
});

describe("the windows follow the seat", () => {
  function windows(r: Harness, list: WireWindow[]) {
    r.socket().deliver({ type: "event", topic: "windows", payload: list, at_ms: 0 });
  }

  it("adopts the snapshot that comes before the seat, and every change while waiting", () => {
    const r = rig();
    r.connect();
    seat(r, elsewhere);
    windows(r, [wire("a")]);
    expect(r.shell.windows.all.value.map((w) => w.id)).toEqual(["a"]);
    windows(r, [wire("a"), wire("b")]);
    expect(r.shell.windows.all.value.map((w) => w.id)).toEqual(["a", "b"]);
  });

  it("keeps a window this device is still opening when an older list crosses it", async () => {
    const r = rig();
    r.connect();
    // Opened the moment the socket was up; the node's snapshot was taken
    // before it heard about the open, and arrives first.
    const opening = r.shell.windows.open("notes");
    await flushPromises();
    const id = r.shell.windows.all.value[0]!.id;
    windows(r, []);
    expect(r.shell.windows.all.value.map((w) => w.id)).toEqual([id]);
    expect(r.shell.windows.focusedId.value).toBe(id);
    await answer(r, "shell.window.open", { status: "ok", value: { id } });
    await opening;
    // Answered, it is the node's like any other: a list without it removes it.
    windows(r, []);
    expect(r.shell.windows.all.value).toHaveLength(0);
  });

  it("focuses the topmost window another device left open, so the compact view shows it", () => {
    const r = rig();
    r.connect();
    const top = { ...wire("b"), desktop: { ...wire("b").desktop, z: 2 } };
    const hidden = { ...wire("c"), desktop: { ...wire("c").desktop, z: 3, minimized: true } };
    windows(r, [wire("a"), top, hidden]);
    expect(r.shell.windows.focusedId.value).toBe("b");
    expect(r.shell.windows.recent.value.map((w) => w.id)).toEqual(["c", "b", "a"]);
  });

  it("gives a window another device opened its app's own minimum size", () => {
    const r = rig();
    r.connect();
    // The node keeps a width only; the shell knows the terminal's real floor.
    windows(r, [{ ...wire("t"), app: "terminal", minWidth: 0, minHeight: 0 }]);
    const known = r.shell.apps.contract("terminal").minSize;
    expect(known.h).toBeGreaterThan(0);
    expect(r.shell.windows.all.value[0]?.minSize).toEqual(known);
  });

  it("ends a split whose half another device closed", () => {
    const r = rig();
    r.connect();
    windows(r, [wire("a"), wire("b")]);
    seat(r, held);
    r.shell.windows.setSplit("a", "b");
    expect(r.shell.windows.split.value).toMatchObject({ left: "a", right: "b" });
    seat(r, elsewhere);
    windows(r, [wire("b")]);
    expect(r.shell.windows.split.value, "the split still names a window that is gone").toBeNull();

    // A split whose halves are both still open is kept.
    r.shell.windows.split.value = { left: "b", right: "b2", fraction: 0.5 };
    windows(r, [wire("b"), wire("b2")]);
    expect(r.shell.windows.split.value).toMatchObject({ left: "b", right: "b2" });
  });

  it("does not adopt the node's echo of the holder's own changes", () => {
    const r = rig();
    r.connect();
    windows(r, [wire("a", 10)]);
    seat(r, held);
    // The holder moved it to 300 locally; the echo still says 10.
    r.shell.windows.place("a", { x: 300, y: 20, w: 600, h: 400 });
    windows(r, [wire("a", 10)]);
    expect(r.shell.windows.all.value[0]?.rect.x).toBe(300);
  });
});

describe("losing the seat", () => {
  it("closes Quick Look, the palette and any open question", async () => {
    const r = rig();
    r.connect();
    seat(r, held);
    await flushPromises();
    r.shell.quicklook.open("/photos/a.png", ["/photos/a.png"]);
    r.shell.ui.paletteOpen.value = true;
    const asked = r.shell.ui.confirm({
      command: "files.delete",
      title: "Delete?",
      detail: "a.png",
      risk: "destructive",
    });
    expect(r.shell.quicklook.visible.value).toBe(true);

    seat(r, elsewhere);
    await flushPromises();
    expect(r.shell.quicklook.visible.value, "Quick Look stayed over the takeover screen").toBe(false);
    expect(r.shell.ui.paletteOpen.value).toBe(false);
    expect(await asked, "a question about a workspace this device lost was not declined").toBe(false);
    expect(r.shell.ui.confirmation.value).toBeNull();
  });

  it("leaves the takeover question alone while the takeover screen is up", async () => {
    const r = rig();
    r.connect();
    seat(r, elsewhere);
    void r.shell.seat.takeOver();
    await flushPromises();
    // Another seat event that changes nothing about showing the workspace.
    seat(r, { ...elsewhere, holder: { ...elsewhere.holder!, connected: false } });
    await flushPromises();
    expect(r.shell.ui.confirmation.value?.command).toBe("seat.take");
  });
});

describe("the installed-app list", () => {
  it("is asked for again when the seat becomes this device's after a refusal", async () => {
    const r = rig();
    r.connect();
    expect(invokes(r, "system.apps")).toHaveLength(1);
    seat(r, elsewhere);
    await answer(r, "system.apps", {
      status: "error",
      error: { code: "seat_not_held", message: "Deckjack is in use on Chrome on Pixel" },
    });
    seat(r, held);
    expect(invokes(r, "system.apps")).toHaveLength(2);
  });

  it("is not asked for twice by a device that was seated from the start", async () => {
    const r = rig();
    r.connect();
    seat(r, held);
    await answer(r, "system.apps", { status: "ok", value: { apps: [] } });
    expect(invokes(r, "system.apps")).toHaveLength(1);
  });
});
