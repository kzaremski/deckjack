// @vitest-environment happy-dom
import { afterEach, describe, expect, it } from "vitest";
import { flushPromises, type VueWrapper } from "@vue/test-utils";

import ConfirmDialog from "./ConfirmDialog.vue";
import SeatTakeover from "./SeatTakeover.vue";
import { harness, mountInShell, type Harness } from "../testing/shell-harness.ts";
import type { SeatStatus } from "../protocol.ts";

/**
 * The takeover screen on the real shell: the seat event a node sends decides
 * what it says, and its button goes through the shell's own confirmation
 * dialog before anything reaches the node.
 */

let wrappers: VueWrapper[] = [];
let rigs: Harness[] = [];

afterEach(() => {
  for (const w of wrappers) w.unmount();
  for (const r of rigs) r.dispose();
  wrappers = [];
  rigs = [];
});

const SINCE = new Date(2026, 10, 21, 14, 7).getTime();

function screen(statuses: SeatStatus[]) {
  const rig = harness();
  const takeover = mountInShell(SeatTakeover, { harness: rig });
  const dialog = mountInShell(ConfirmDialog, { harness: rig });
  wrappers.push(takeover.wrapper, dialog.wrapper);
  rigs.push(rig);
  rig.connect();
  for (const payload of statuses) {
    rig.socket().deliver({ type: "event", topic: "seat", payload, at_ms: 0 });
  }
  return { rig, wrapper: takeover.wrapper, dialog: dialog.wrapper };
}

/** The screen mounted into the document, so focus can be checked. */
function screenAttached(statuses: SeatStatus[]) {
  const rig = harness();
  rig.connect();
  for (const payload of statuses) {
    rig.socket().deliver({ type: "event", topic: "seat", payload, at_ms: 0 });
  }
  const host = document.createElement("div");
  document.body.appendChild(host);
  const takeover = mountInShell(SeatTakeover, { harness: rig, attachTo: host });
  wrappers.push(takeover.wrapper);
  rigs.push(rig);
  return { rig, wrapper: takeover.wrapper };
}

const pixel: SeatStatus = {
  held: false,
  claimable: false,
  holder: { device: "Chrome on Pixel", since_ms: SINCE, connected: true },
};
const mine: SeatStatus = {
  held: true,
  claimable: false,
  holder: { device: "Firefox on Linux", since_ms: SINCE, connected: true },
};

function button(wrapper: VueWrapper, text: string) {
  const found = wrapper.findAll("button").find((b) => b.text() === text);
  expect(found, `no ${text} button`).toBeTruthy();
  return found!;
}

describe("the takeover screen", () => {
  it("names the device that has it and since when", async () => {
    const { wrapper } = screen([pixel]);
    await flushPromises();
    expect(wrapper.find("h1").text()).toBe("Deckjack is in use on Chrome on Pixel");
    expect(wrapper.find(".since").text()).toBe("Since 14:07");
    button(wrapper, "Take over");
  });

  it("says the seat was taken, and offers it back, when this device had it", async () => {
    const { wrapper } = screen([mine, pixel]);
    await flushPromises();
    expect(wrapper.find("h1").text()).toBe("Taken over by Chrome on Pixel");
    button(wrapper, "Take back");
  });

  it("says a holder whose connection dropped is reconnecting", async () => {
    const { wrapper } = screen([{ ...pixel, holder: { ...pixel.holder!, connected: false } }]);
    await flushPromises();
    expect(wrapper.find(".since").text()).toContain("reconnecting");
  });

  it("asks through the confirmation dialog before taking the seat", async () => {
    const { rig, wrapper, dialog } = screen([pixel]);
    await flushPromises();
    await button(wrapper, "Take over").trigger("click");
    await flushPromises();
    expect(
      rig.socket().frames().some((f) => f.command === "seat.take"),
      "the seat was taken before the operator answered",
    ).toBe(false);

    await button(dialog, "Take over").trigger("click");
    await flushPromises();
    const take = rig
      .socket()
      .frames()
      .find((f) => f.type === "invoke" && f.command === "seat.take");
    expect(take?.params).toEqual({ device: "Firefox on Linux" });
  });

  it("moves focus to its heading, so the change is announced", async () => {
    const { wrapper } = screenAttached([pixel]);
    await flushPromises();
    expect(document.activeElement).toBe(wrapper.find("h1").element);
  });

  it("offers the kill switch, which the node runs whoever holds the seat", async () => {
    const { rig, wrapper } = screen([pixel]);
    await flushPromises();
    await button(wrapper, "Lock down this workspace").trigger("click");
    await flushPromises();
    const asked = rig.shell.ui.confirmation.value;
    expect(asked?.command).toBe("system.lockdown");
    asked!.resolve(true);
    await flushPromises();
    expect(
      rig
        .socket()
        .frames()
        .some((f) => f.type === "invoke" && f.command === "system.lockdown"),
      "the lockdown never reached the node",
    ).toBe(true);
  });

  it("renames this device", async () => {
    const { rig, wrapper } = screen([pixel]);
    await flushPromises();
    await button(wrapper, "Rename").trigger("click");
    await wrapper.find("input").setValue("Kitchen tablet");
    await wrapper.find("form").trigger("submit");
    await flushPromises();
    expect(wrapper.find('[data-testid="seat-device"]').text()).toBe("Kitchen tablet");
    expect(rig.shell.seat.device.label.value).toBe("Kitchen tablet");
  });
});
