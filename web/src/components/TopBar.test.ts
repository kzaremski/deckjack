// @vitest-environment happy-dom
import { readFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

import { afterEach, describe, expect, it } from "vitest";
import { flushPromises, type VueWrapper } from "@vue/test-utils";

import TopBar from "./TopBar.vue";
import {
  harness,
  mountInShell,
  type FakeSocket,
  type Harness,
  type HarnessOptions,
} from "../testing/shell-harness.ts";
import type { AppId } from "../stores/apps.ts";

/**
 * What the bar owes the operator.
 *
 * The bar is the one surface that is always on screen, which makes it the
 * worst possible place for a confident lie. Two of its claims are about a
 * machine that is not this one — which node is answering, and how long a round
 * trip to it takes — and both have a cached value sitting right next to them
 * that would read as plausible long after it stopped being true. So most of
 * what is asserted here is the bar *withdrawing* a claim: no node name once
 * the socket is dead, no latency figure once nothing has come back.
 *
 * Everything is driven through the real shell (`shell-harness.ts`): a real
 * bus with a test-driven socket, and the real window store. That matters more
 * here than anywhere else in the suite, because this project's recurring
 * defect is a module that passes in isolation and is then never wired up, or a
 * store that is right while the component above it renders something false.
 * A stubbed `shell` would agree with whatever the template asked for and prove
 * nothing. The windows in these tests are therefore opened by asking the node
 * and answering as a node would, so a window only exists here if it would
 * exist at runtime.
 *
 * Assertions are on rendered text and on accessible names and states — what
 * the operator reads and presses — never on the component's internals.
 */

const wrappers: VueWrapper[] = [];

afterEach(() => {
  // TopBar starts a `setInterval` for its clock in `onMounted`; unmounting is
  // what stops it, and a bar left mounted across tests keeps ticking.
  while (wrappers.length) wrappers.pop()?.unmount();
});

function mountBar(rig: Harness) {
  const { wrapper } = mountInShell(TopBar, { harness: rig });
  wrappers.push(wrapper);
  return wrapper;
}

/** Answers every invoke the shell has sent that is still waiting, as a node would. */
function answerInvokes(socket: FakeSocket, answered: Set<string>): void {
  for (const frame of socket.frames()) {
    if (frame.type !== "invoke") continue;
    const id = frame.id;
    if (typeof id !== "string" || answered.has(id)) continue;
    answered.add(id);
    socket.deliver({
      type: "response",
      id,
      outcome: { status: "ok", value: null },
      elapsed_ms: 1,
    });
  }
}

/**
 * A shell with a node on the other end, plus the one thing a test needs it
 * for: opening a window the node actually confirmed. The window store refuses
 * to open anything while detached, so a window that exists here is one that
 * survived the round trip.
 */
function live(options: HarnessOptions = {}) {
  const rig = harness(options);
  const answered = new Set<string>();
  rig.connect("pocket");
  return {
    rig,
    async open(app: AppId): Promise<string> {
      const opening = rig.shell.windows.open(app);
      answerInvokes(rig.socket(), answered);
      const id = await opening;
      await flushPromises();
      return id;
    },
    /**
     * Answers whatever the shell has asked the node for since the last call.
     *
     * `open` above answers the invoke it starts itself. This is for work a
     * *click* started, where the test has no promise to hold on to.
     */
    async answer(): Promise<void> {
      answerInvokes(rig.socket(), answered);
      await flushPromises();
    },
  };
}

/**
 * Establishes a measured round trip of about `ms`.
 *
 * The bus only learns a latency from a pong it matched to a ping it sent, so
 * this has to beat, wait, and answer. The bus clock is the harness's, not the
 * wall clock, which is why the figure is exact.
 */
function measureRoundTrip(rig: Harness, ms: number): void {
  rig.advance(8_000); // the first heartbeat, which sends the ping
  rig.advance(ms); // time the node spends thinking
  rig.socket().pong();
}

/** Long enough that the bus has to conclude the node is gone (26s of silence). */
const UNTIL_DECLARED_DEAD_MS = 40_000;

/** Finds a taskbar button by what it says, the way the operator picks it out. */
function dockButton(wrapper: VueWrapper, label: string) {
  const button = wrapper.findAll("button").find((b) => b.text().includes(label));
  if (!button) throw new Error(`no taskbar button reading "${label}"`);
  return button;
}

const PHONE: HarnessOptions = { size: { w: 420, h: 900 }, coarsePointer: true };

describe("the node pill", () => {
  it("names the node while the connection is genuinely open", async () => {
    // The counterpart to every withdrawal test below: a bar that said
    // "disconnected" unconditionally would pass those and be useless.
    const { rig } = live();
    const wrapper = mountBar(rig);
    await wrapper.vm.$nextTick();

    expect(wrapper.text(), "the bar will not name the node that is answering").toContain(
      "pocket",
    );
    expect(wrapper.text(), "a live connection is being reported as a broken one").not.toMatch(
      /reconnecting|disconnected|unreachable|offline/,
    );
  });

  it("stops claiming a live node, and stops quoting its latency, once the socket has gone quiet", async () => {
    // The specific lie: "pocket · gen 1 · 40ms" in a calm, confident steady
    // state, because both figures were cached from a session that has since
    // ended. The node's name outlives the connection by design, so the status
    // has to win over it, and the last round trip has to be withdrawn rather
    // than left standing as a current measurement.
    const { rig } = live();
    const wrapper = mountBar(rig);
    measureRoundTrip(rig, 40);
    await wrapper.vm.$nextTick();
    expect(wrapper.text(), "the pill never reached its healthy state to begin with").toMatch(
      /pocket · gen \d+/,
    );
    expect(wrapper.text(), "no round trip was measured, so nothing can go stale").toContain(
      "40ms",
    );

    rig.advance(UNTIL_DECLARED_DEAD_MS);
    await wrapper.vm.$nextTick();

    expect(
      wrapper.text(),
      "the bar still claims a live generation on a node that stopped answering",
    ).not.toMatch(/gen \d+/);
    expect(wrapper.text(), "the bar does not say the connection is being retried").toContain(
      "reconnecting",
    );
    expect(
      wrapper.text(),
      "the bar still shows the last round trip it ever managed, as if it were current",
    ).not.toMatch(/\d+ms/);
  });

  it("keeps naming the node that went quiet, so the operator knows which one dropped", async () => {
    // The withdrawal test above checks that "gen N" is gone and the word
    // "reconnecting" is there, which a pill reading "node · reconnecting"
    // satisfies while having lost the one fact that distinguishes two nodes.
    // Pinning the whole string says it in one assertion: the name is kept,
    // the generation is gone, and no round trip is quoted beside it.
    const { rig } = live();
    const wrapper = mountBar(rig);
    measureRoundTrip(rig, 40);
    rig.advance(UNTIL_DECLARED_DEAD_MS);
    await wrapper.vm.$nextTick();

    expect(
      wrapper.get(".node").text(),
      "the degraded pill does not read exactly as the operator is owed",
    ).toBe("pocket · reconnecting");
  });

  it("withdraws a round trip whose connection is no longer open, whatever the store holds", async () => {
    // The bar's own `status === "open"` gate is what this is about, and the
    // suite cannot see it from the outside: the bus nulls `rtt` on both of
    // its own ways down, so a bar with no gate at all passes anyway. Putting
    // a measurement back after the connection has gone asks the component
    // directly, and models the obvious "show the last known latency" change
    // to the store that would otherwise reach the pill unopposed.
    const { rig } = live();
    const wrapper = mountBar(rig);
    measureRoundTrip(rig, 40);
    await wrapper.vm.$nextTick();
    expect(wrapper.get(".rtt").text(), "no round trip was reported to begin with").toBe("40ms");

    rig.advance(UNTIL_DECLARED_DEAD_MS);
    rig.shell.bus.rtt.value = 40;
    await wrapper.vm.$nextTick();

    expect(
      wrapper.find(".rtt").exists(),
      "the bar quotes a round trip measured over a connection that is gone",
    ).toBe(false);
  });

  it("says this device is offline instead of naming a node it cannot reach", async () => {
    const { rig } = live();
    const wrapper = mountBar(rig);

    rig.setOnline(false);
    await wrapper.vm.$nextTick();

    expect(wrapper.text(), "an offline device is not told it is offline").toContain("offline");
    expect(
      wrapper.text(),
      "the bar names a node while the device has no network to reach it over",
    ).not.toContain("pocket");
  });

  it("reports only a round trip it actually measured", async () => {
    const { rig } = live();
    const wrapper = mountBar(rig);
    await wrapper.vm.$nextTick();

    // Nothing has come back yet, so there is no honest figure to show.
    expect(wrapper.text(), "a latency appeared before any round trip completed").not.toMatch(
      /\d+ms/,
    );

    measureRoundTrip(rig, 40);
    await wrapper.vm.$nextTick();

    // Exact, because the harness owns the bus clock: 40ms is the time the
    // node took, not an interval the test happens to have observed.
    expect(wrapper.text(), "the measured round trip is not reported").toContain("40ms");
  });

  it("shows no round trip at all until it has measured one", async () => {
    // The sibling test asserts this with `/\d+ms/`, which is blind to the way
    // the gate actually breaks: an ungated figure renders the literal
    // "nullms", and that has no digits in it. So this asks whether the
    // round-trip element is on screen at all, and pins the pill's whole text.
    const { rig } = live();
    const wrapper = mountBar(rig);
    await wrapper.vm.$nextTick();

    expect(
      wrapper.find(".rtt").exists(),
      "a round trip is reported on a connection nothing has come back over yet",
    ).toBe(false);
    expect(
      wrapper.get(".node").text(),
      "the pill says something about a round trip before there is one to report",
    ).toBe("pocket · gen 1");

    measureRoundTrip(rig, 40);
    await wrapper.vm.$nextTick();

    expect(wrapper.get(".rtt").text(), "the measured round trip is not reported").toBe("40ms");
  });

  it("does not quote a round trip while telling the operator the device is offline", async () => {
    // A number next to the word "offline" reads as "this is how fast the node
    // is right now", and it is a figure from before the network went away.
    //
    // RED against the current component, and deliberately left so: this is a
    // defect, not a misread promise. The bar withdraws the node's name when
    // the device goes offline but keeps the last measured round trip, because
    // `latency` is gated on `bus.status` alone and losing the network does not
    // change the status — the socket is only condemned later, by the
    // heartbeat. The pill therefore reads "offline 40ms".
    const { rig } = live();
    const wrapper = mountBar(rig);
    measureRoundTrip(rig, 40);
    await wrapper.vm.$nextTick();

    rig.setOnline(false);
    await wrapper.vm.$nextTick();

    expect(
      wrapper.text(),
      "the pill says offline and quotes a latency in the same breath",
    ).not.toMatch(/\d+ms/);
  });
});

describe("what is running", () => {
  it("names the window the operator is in, on a phone", async () => {
    const node = live(PHONE);
    const wrapper = mountBar(node.rig);

    await node.open("terminal");
    await wrapper.vm.$nextTick();
    const switcher = wrapper.get('[aria-label="Switch window"]');
    expect(switcher.text(), "the bar does not say which window you are in").toContain("Terminal");

    await node.open("notes");
    await wrapper.vm.$nextTick();
    expect(switcher.text(), "the bar still names the window you left").not.toContain("Terminal");
    expect(switcher.text(), "the bar does not follow focus to the new window").toContain("Notes");
  });

  it("follows focus rather than the focus history, on a phone", async () => {
    // `recent` is the focus history and it keeps a minimized window at its
    // head, because minimizing does not pop it. A bar that reads the history
    // therefore names a window that is no longer on screen while a different
    // one has focus, and its title then disagrees with its own switcher.
    const node = live(PHONE);
    const wrapper = mountBar(node.rig);

    await node.open("terminal");
    const notes = await node.open("notes");
    node.rig.shell.windows.minimize(notes);
    await wrapper.vm.$nextTick();

    const switcher = wrapper.get('[aria-label="Switch window"]');
    expect(switcher.text(), "the bar names the window it just put away").not.toContain("Notes");
    expect(switcher.text(), "the bar does not name the window that has focus").toContain(
      "Terminal",
    );
  });

  it("does not show a window the node never confirmed", async () => {
    // The failure this guards is the project's own: a window that exists only
    // in this browser, named in the one place the operator always looks, while
    // the node it is supposed to be running on knows nothing about it.
    const rig = harness(PHONE); // never connected
    const wrapper = mountBar(rig);

    await rig.shell.commands.dispatch("app.open.terminal");
    await flushPromises();
    await wrapper.vm.$nextTick();

    expect(
      wrapper.get('[aria-label="Switch window"]').text(),
      "the bar names a window the node refused to open",
    ).not.toContain("Terminal");
  });

  it("focuses an app's window when its taskbar button is pressed", async () => {
    const node = live();
    const wrapper = mountBar(node.rig);

    await node.open("terminal");
    await node.open("notes");
    await wrapper.vm.$nextTick();
    expect(
      dockButton(wrapper, "Notes").attributes("aria-pressed"),
      "the most recently opened window is not shown as the focused one",
    ).toBe("true");
    expect(
      dockButton(wrapper, "Terminal").attributes("aria-pressed"),
      "two windows are both shown as focused",
    ).toBe("false");

    await dockButton(wrapper, "Terminal").trigger("click");
    await wrapper.vm.$nextTick();

    expect(
      dockButton(wrapper, "Terminal").attributes("aria-pressed"),
      "pressing a running app's button did not focus its window",
    ).toBe("true");
    expect(
      dockButton(wrapper, "Notes").attributes("aria-pressed"),
      "the window that lost focus is still shown as focused",
    ).toBe("false");
  });

  it("launches an app whose button is pressed while it is not running", async () => {
    // The taskbar is the launcher as well as the switcher, and on a fresh
    // session almost every item is in this state. A dock button that does
    // nothing here -- no window, no command, no error -- is the failure
    // nobody reports, because there is nothing to report.
    const node = live();
    const wrapper = mountBar(node.rig);
    await wrapper.vm.$nextTick();

    await dockButton(wrapper, "Terminal").trigger("click");
    await node.answer();
    await wrapper.vm.$nextTick();

    expect(
      node.rig.shell.windows.all.value.map((w) => w.app),
      "pressing the button for an app that is not running opened nothing",
    ).toContain("terminal");
    expect(
      dockButton(wrapper, "Terminal").attributes("aria-pressed"),
      "the app was launched behind whatever the operator was already looking at",
    ).toBe("true");
  });

  it("puts away the window the operator is looking at, not another of that app's", async () => {
    // Two windows of one app is where the lit button and the window it acts
    // on can come apart: the button lights because *some* Terminal has focus,
    // so if it minimizes a different Terminal the operator presses a lit
    // button, watches nothing move, and presses it again forever.
    const node = live();
    const wrapper = mountBar(node.rig);

    const first = await node.open("terminal");
    const second = await node.open("terminal");
    await wrapper.vm.$nextTick();
    expect(
      node.rig.shell.windows.focused.value?.id,
      "the second window never took focus, so there is nothing to confuse",
    ).toBe(second);

    const onScreen = () => node.rig.shell.windows.visible.value.map((w) => w.id);

    await dockButton(wrapper, "Terminal").trigger("click");
    await wrapper.vm.$nextTick();

    expect(onScreen(), "the button put away a background window and left the focused one up").toEqual(
      [first],
    );

    await dockButton(wrapper, "Terminal").trigger("click");
    await wrapper.vm.$nextTick();

    expect(onScreen(), "pressing the button again cannot put away what is left").toEqual([]);
    expect(
      dockButton(wrapper, "Terminal").attributes("aria-pressed"),
      "nothing of the app is on screen and its button still reads as focused",
    ).toBe("false");
  });

  it("shows the Assistant's pending approvals as a count on its own dock button", async () => {
    // The Assistant surfaces rather than taking over, which makes this badge
    // the whole of what the operator gets to tell them the Assistant is
    // blocked waiting on them.
    const node = live();
    const wrapper = mountBar(node.rig);
    node.rig.shell.inboxPending.value = 2;
    await wrapper.vm.$nextTick();

    const assistant = dockButton(wrapper, "Assistant");
    expect(assistant.text(), "nothing on the bar says the Assistant is waiting").toContain("2");
    expect(
      assistant.text(),
      "a screen reader is given a bare number with nothing to say what it counts",
    ).toContain("approvals waiting");
    expect(
      dockButton(wrapper, "Files").find(".badge").exists(),
      "an app with no Inbox is wearing the Assistant's count",
    ).toBe(false);
  });

  it("puts no badge on a bar with nothing waiting", async () => {
    // The mirror of the test above: a badge that always renders is a
    // warning-colored "0" on every item and a false "approvals waiting"
    // announced for apps that have no Inbox at all.
    const node = live();
    const wrapper = mountBar(node.rig);
    await wrapper.vm.$nextTick();

    expect(
      wrapper.findAll(".badge").length,
      "a badge rendered for an app with nothing pending",
    ).toBe(0);
    expect(
      wrapper.text(),
      "the bar announces approvals that nobody is waiting on",
    ).not.toContain("approvals waiting");
  });
});

describe("the lockdown button", () => {
  it("actually locks the workspace down", async () => {
    // The worst defect this bar can ship: the operator presses the button
    // labeled "Lock down this workspace", believes the workspace is secured,
    // and walks away from one that is not. So this follows the press all the
    // way to the frame the node receives, rather than trusting the label.
    const { rig } = live();
    const wrapper = mountBar(rig);
    await wrapper.vm.$nextTick();

    await wrapper.get('[aria-label="Lock down this workspace"]').trigger("click");
    await flushPromises();

    const asked = rig.shell.ui.confirmation.value;
    expect(asked?.command, "the button did not even ask to lock the workspace down").toBe(
      "system.lockdown",
    );
    asked?.resolve(true);
    await flushPromises();

    expect(
      rig
        .socket()
        .frames()
        .filter((frame) => frame.type === "invoke")
        .map((frame) => frame.command),
      "the operator confirmed a lockdown that never reached the node",
    ).toContain("system.lockdown");
    expect(
      rig.shell.ui.paletteOpen.value,
      "the lockdown button opened the palette instead",
    ).toBe(false);
  });
});

describe("the layout toggle", () => {
  it("switches between floating and tiling, and offers the mode it is not in", async () => {
    // The button's accessible name is the promise: it tells the operator what
    // pressing it will do, so it has to change when the mode does. A toggle
    // that still offers tiling after tiling it is a toggle that did nothing.
    const { rig } = live();
    const wrapper = mountBar(rig);
    await wrapper.vm.$nextTick();

    const toTiling = wrapper.get('[aria-label="Switch to tiling layout"]');
    expect(
      toTiling.attributes("aria-pressed"),
      "the shell starts in floating, so the toggle must not read as engaged",
    ).toBe("false");

    await toTiling.trigger("click");
    await flushPromises();
    await wrapper.vm.$nextTick();

    const toFloating = wrapper.get('[aria-label="Switch to floating windows"]');
    expect(
      toFloating.attributes("aria-pressed"),
      "the bar is in tiling but the toggle does not read as engaged",
    ).toBe("true");
    expect(
      wrapper.find('[aria-label="Switch to tiling layout"]').exists(),
      "the toggle still offers to tile a layout that is already tiled",
    ).toBe(false);

    await toFloating.trigger("click");
    await flushPromises();
    await wrapper.vm.$nextTick();

    expect(
      wrapper.find('[aria-label="Switch to tiling layout"]').exists(),
      "tiling cannot be turned back off from the bar",
    ).toBe(true);
  });
});

describe("the bar is chrome, not content", () => {
  it("is not selectable text, so a long press on a launcher does not offer to copy it", () => {
    // Asserted against the declaration rather than a computed style: vitest
    // does not process a component's CSS, so `getComputedStyle` would report
    // nothing either way and the assertion would pass for a bar that had no
    // rule at all. The declaration is the only honest form of this promise
    // available here.
    // Spelled with `join` rather than `new URL(..., import.meta.url)`, which
    // Vite rewrites into an asset request before Node ever sees it.
    const source = readFileSync(join(dirname(fileURLToPath(import.meta.url)), "TopBar.vue"), "utf8");
    const barRule = /\.bar\s*\{([^}]*)\}/.exec(source)?.[1] ?? "";

    expect(barRule, "the bar's own style rule was not found at all").not.toBe("");
    expect(barRule, "dragging across the bar selects its labels as if they were text").toMatch(
      /user-select:\s*none/,
    );
    expect(
      barRule,
      "a long press on iOS still offers the callout menu over a launcher",
    ).toMatch(/-webkit-touch-callout:\s*none/);
  });
});
