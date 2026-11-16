// @vitest-environment happy-dom
import { afterEach, describe, expect, it, vi } from "vitest";
import { nextTick } from "vue";
import type { VueWrapper } from "@vue/test-utils";

import App from "../App.vue";
import ConnectionCard from "./ConnectionCard.vue";
import {
  harness,
  mountInShell,
  type Harness,
  type HarnessOptions,
} from "../testing/shell-harness.ts";

/**
 * What the operator is told when the shell cannot reach its node.
 *
 * The node holds the work, so while it is unreachable there is nothing on
 * screen worth clicking. This card is the only thing standing between the
 * operator and a workspace that looks alive and is not, and the promises it
 * owes them are about *words*: which of two failures happened, how long it has
 * been true, and whether anything is still trying. Telling someone their wifi
 * is down when their workstation is down sends them to restart a router while
 * their work sits unreachable.
 *
 * So every test here drives the real bus into a real failure and then reads
 * the card the way the operator does — the text on it and the button they can
 * press. Nothing asserts on a computed ref: a card that got the right answer
 * internally and rendered the wrong sentence has still lied, and that is
 * precisely the failure a store-level test cannot see.
 */

/**
 * The harness's bus clock starts here, and the card reads `Date.now()`
 * directly rather than through an injected clock.
 *
 * In a browser those are one clock. In a test they are only one clock if the
 * test makes them agree, and a card whose idea of "now" sits 1.7e12 ms ahead
 * of the bus's would report nonsense for every duration on it. So wall time is
 * pinned to the bus's epoch and the two are moved together.
 */
const EPOCH = 1_000;

function rigAt(options: HarnessOptions = {}): Harness {
  vi.useFakeTimers({ now: EPOCH });
  return harness(options);
}

/** Move the bus's clock and the wall clock together, as one real clock does. */
async function step(rig: Harness, ms: number): Promise<void> {
  rig.advance(ms);
  // This also runs the card's own one-second interval, which is what makes
  // the durations on it move.
  vi.advanceTimersByTime(ms);
  await nextTick();
}

/** The labeled facts on the card, keyed by the label the operator reads. */
function facts(wrapper: VueWrapper): Record<string, string> {
  const labels = wrapper.findAll("dt");
  const values = wrapper.findAll("dd");
  const out: Record<string, string> = {};
  labels.forEach((label, i) => {
    const value = values[i];
    if (value) out[label.text()] = value.text();
  });
  return out;
}

function fact(wrapper: VueWrapper, label: string): string {
  const value = facts(wrapper)[label];
  if (value === undefined) throw new Error(`the card never showed a "${label}"`);
  return value;
}

/** The button by its label, because that is how the operator finds it. */
function tryNow(wrapper: VueWrapper) {
  const button = wrapper.findAll("button").find((b) => /try now/i.test(b.text()));
  if (!button) throw new Error("the card offered no way to retry the connection");
  return button;
}

/**
 * A shell stuck in a long backoff.
 *
 * One failed attempt is retried a quarter of a second later, and a countdown
 * that can only ever read "1s" cannot be shown to be counting. Failing
 * repeatedly walks the backoff up to its cap, which is also the state an
 * operator with a node that is genuinely down will be looking at.
 */
async function stuckInBackoff(rig: Harness): Promise<void> {
  rig.shell.bus.connect();
  for (let i = 0; i < 5; i++) {
    rig.socket().close(); // nothing there to answer
    await step(rig, 11_000); // wait out whatever backoff that earned
  }
  rig.socket().close();
  await nextTick();
}

afterEach(() => {
  vi.useRealTimers();
});

describe("a shell with no node", () => {
  it("says so for as long as it is true, and stops the moment the node answers", async () => {
    const rig = rigAt(); // built, but nothing has connected yet
    const { wrapper } = mountInShell(ConnectionCard, { harness: rig });

    expect(
      wrapper.find('[role="alert"]').exists(),
      "the shell said nothing about having no node",
    ).toBe(true);

    rig.connect("pocket");
    await nextTick();

    expect(
      wrapper.find('[role="alert"]').exists(),
      "the card stayed over a workspace that works",
    ).toBe(false);
    expect(wrapper.text(), "the card left text behind over a live node").toBe("");
  });

  it("interrupts a screen reader instead of waiting for a pause in it", async () => {
    // Someone who cannot see the card is about to operate a workspace that is
    // not there. "Polite" means they find out after they have tried.
    const rig = rigAt();
    const { wrapper } = mountInShell(ConnectionCard, { harness: rig });

    const alert = wrapper.get('[role="alert"]');
    expect(
      alert.attributes("aria-live"),
      "a dead workspace was announced politely, if at all",
    ).toBe("assertive");
  });
});

describe("which of the two failures it was", () => {
  it("blames this device when this device has no network", async () => {
    const rig = rigAt({ online: false });
    const { wrapper } = mountInShell(ConnectionCard, { harness: rig });

    const text = wrapper.text();
    expect(text, "an operator with no network was not told that").toMatch(
      /this device is offline/i,
    );
    expect(text, "nothing said what the shell is waiting for").toMatch(/waiting for a network/i);
    // The pip is the one thing on the card that is not words, and the
    // stylesheet reads it off this attribute: red for a dead network, amber
    // and pulsing for something still trying. A pip that disagrees with the
    // heading above it is the same lie in color.
    expect(
      wrapper.get(".card").attributes("data-state"),
      "the pip told a different story than the heading",
    ).toBe("offline");
  });

  it("stops blaming the network as soon as the network is back", async () => {
    const rig = rigAt({ online: false });
    const { wrapper } = mountInShell(ConnectionCard, { harness: rig });
    expect(wrapper.text()).toMatch(/offline/i);

    rig.setOnline(true);
    await nextTick();

    expect(
      wrapper.find('[role="alert"]').exists(),
      "the card left before the node had answered",
    ).toBe(true);
    expect(
      wrapper.text(),
      "the operator was still being sent to fix a network that is fine",
    ).not.toMatch(/offline/i);
  });

  it("does not blame the network when the network is fine and the node went quiet", async () => {
    // The expensive mistake: a workstation that is down, reported as the
    // operator's wifi. `navigator.onLine` says true here, and saying true is
    // never evidence of anything — a captive portal says it too.
    const rig = rigAt({ online: true });
    rig.connect("pocket");
    const { wrapper } = mountInShell(ConnectionCard, { harness: rig });

    expect(
      wrapper.find('[role="alert"]').exists(),
      "a live node was reported as unreachable",
    ).toBe(false);

    await step(rig, 28_000); // past the bus's silence budget

    const text = wrapper.text();
    expect(text, "the node went quiet and the shell never mentioned it").toMatch(/\S/);
    expect(text, "a working network was blamed for a node being down").not.toMatch(/offline/i);
    expect(text, "the operator was not told which machine is unreachable").toMatch(
      /cannot reach pocket/i,
    );
  });

  it("says the node is not answering when an attempt failed outright", async () => {
    const rig = rigAt({ online: true });
    rig.shell.bus.connect();
    rig.socket().close(); // the connection never came up at all
    const { wrapper } = mountInShell(ConnectionCard, { harness: rig });

    expect(wrapper.text(), "a failed attempt was described as something else").toMatch(
      /node not answering/i,
    );
  });

  it("calls it disconnected, and promises no attempt, when nothing is retrying", async () => {
    const rig = rigAt();
    rig.shell.bus.close(); // closed before anything was opened: nothing retries
    const { wrapper } = mountInShell(ConnectionCard, { harness: rig });
    await nextTick();

    const text = wrapper.text();
    expect(
      wrapper.find('[role="alert"]').exists(),
      "the shell went quiet about a connection it had abandoned",
    ).toBe(true);
    expect(text, "an abandoned connection read like one still being retried").toMatch(
      /disconnected/i,
    );
    expect(text, "nothing said that no retry is coming").toMatch(/nothing is retrying/i);
    expect(
      facts(wrapper),
      "a countdown to an attempt that will never be made",
    ).not.toHaveProperty("Next try");
    expect(
      wrapper.get(".card").attributes("data-state"),
      "an abandoned connection was given the pip of one still trying",
    ).toBe("closed");
  });

  it("promises no attempt after the shell closes a connection it was retrying", async () => {
    // The scenario the other "nothing is retrying" test cannot reach: it
    // closes before anything was ever opened, so there is no pending retry
    // for the shutdown to have to call off. Here a real attempt has already
    // failed and a real retry is scheduled when the shell gives the order.
    const rig = rigAt();
    const { wrapper } = mountInShell(ConnectionCard, { harness: rig });
    rig.shell.bus.connect();
    rig.socket().close(); // one failed attempt, so a retry is now pending
    await nextTick();
    expect(
      facts(wrapper),
      "this test needs a retry in flight for the shutdown to call off",
    ).toHaveProperty("Next try");

    rig.shell.bus.close(); // the shell is shutting this connection down
    await nextTick();

    expect(wrapper.text(), "nothing said that no retry is coming").toMatch(
      /nothing is retrying/i,
    );
    expect(
      facts(wrapper),
      "a countdown to a retry of a connection the shell had closed",
    ).not.toHaveProperty("Next try");
    // The one assertion in this file on a store ref rather than on words, and
    // deliberately: the card refuses to count down on a closed connection, so
    // the card alone cannot show whether the bus still believes an attempt is
    // due. Two guards implement this promise and neither is allowed to be the
    // only one, because the next change to either reveals the other.
    expect(
      rig.shell.bus.retryAt.value,
      "the bus still had an attempt on the books for a connection it closed",
    ).toBeNull();
  });

  it("shows no countdown while the status says nothing is retrying", async () => {
    const rig = rigAt();
    const { wrapper } = mountInShell(ConnectionCard, { harness: rig });

    // The card's own half of the guard above, pinned on its own. The bus will
    // not leave a retry on the books while closed, so this state is written
    // by hand: what is being tested is that the card would not render a
    // countdown even if it were handed one.
    rig.shell.bus.retryAt.value = EPOCH + 1_000;
    rig.shell.bus.status.value = "closed";
    await nextTick();

    expect(wrapper.text(), "an abandoned connection read like one still trying").toMatch(
      /nothing is retrying/i,
    );
    expect(
      facts(wrapper),
      'a countdown rendered directly under the words "nothing is retrying it"',
    ).not.toHaveProperty("Next try");
  });

  it("leaves a closed connection closed when the network comes back", async () => {
    // The network returning is worth one immediate attempt on a connection
    // that is trying to come up. On one the shell shut down it is not: the
    // operator would get a live node nobody asked for, and "closed" would
    // stop meaning "nothing is retrying this".
    const rig = rigAt();
    rig.connect("pocket");
    const { wrapper } = mountInShell(ConnectionCard, { harness: rig });
    rig.shell.bus.close();
    await nextTick();

    const before = rig.sockets.length;
    rig.setOnline(false);
    rig.setOnline(true);
    await nextTick();

    expect(
      rig.sockets.length,
      "a connection the shell closed reopened itself when the network returned",
    ).toBe(before);
    expect(wrapper.text(), "a reopened connection stopped reading as a closed one").toMatch(
      /nothing is retrying/i,
    );
  });

  it("says the shell gave up, not that it is waiting for wifi, when both are true", async () => {
    // Offline and closed at once. The two sentences disagree about what
    // happens next — one says the shell resumes when the network returns, and
    // for a closed connection that is not true — so the operator must be told
    // the one that will still be true after they have fixed the wifi.
    const rig = rigAt({ online: false });
    const { wrapper } = mountInShell(ConnectionCard, { harness: rig });
    rig.shell.bus.close();
    await nextTick();

    expect(
      wrapper.get("h2").text(),
      "an abandoned connection was reported as a network problem",
    ).toBe("Disconnected");
    expect(wrapper.text(), "nothing said that no retry is coming").toMatch(
      /nothing is retrying/i,
    );
    expect(
      wrapper.text(),
      "the operator was promised a recovery that will never happen",
    ).not.toMatch(/waiting for a network/i);
  });

  it("calls it reconnecting only while an attempt is actually in flight", async () => {
    const rig = rigAt();
    rig.connect("pocket");
    const { wrapper } = mountInShell(ConnectionCard, { harness: rig });

    await step(rig, 28_000); // the node goes quiet, and the retry has fired

    expect(
      fact(wrapper, "Next try"),
      "this test needs an attempt under way, not one being waited for",
    ).toBe("now");
    expect(
      wrapper.get("h2").text(),
      "an attempt in flight read the same as a node declared down",
    ).toMatch(/reconnecting/i);
  });

  it("does not claim an attempt is in flight while it is counting down to one", async () => {
    const rig = rigAt();
    rig.connect("pocket");
    const { wrapper } = mountInShell(ConnectionCard, { harness: rig });

    // Stopped between the bus condemning the node and its retry coming due.
    // The bus's status reads "connecting" for the whole of that wait, with no
    // socket open, which is the state an operator spends an outage in.
    await step(rig, 26_300);

    expect(
      fact(wrapper, "Next try"),
      "this test needs a wait that has not expired yet",
    ).not.toBe("now");
    expect(
      wrapper.get("h2").text(),
      "the card claimed an attempt was in flight beside the countdown to it",
    ).not.toMatch(/reconnecting/i);
  });

  /**
   * RED, and left red: this documents a defect in `bus.close()`, not in the
   * card.
   *
   * `close()` sets the status to "closed" and then calls `socket.close()`
   * without detaching the socket's handlers first — the thing `giveUp()` is
   * careful to do, for this exact reason. The socket's own `onclose` then runs
   * and rewrites the status to "detached" and schedules a reconnect, so a
   * connection the shell deliberately shut down comes back up and the one
   * state that means "nothing is retrying" is unreachable whenever a socket
   * was open. What the operator reads is the proof: after the shell closed the
   * connection, the card says the node is not answering and counts down to an
   * attempt nobody asked for.
   */
  it("stays disconnected after the shell closes the connection", async () => {
    const rig = rigAt();
    rig.connect("pocket");
    rig.shell.bus.close(); // the shell is shutting this connection down
    const { wrapper } = mountInShell(ConnectionCard, { harness: rig });
    await nextTick();

    expect(wrapper.text(), "a connection the shell closed read as one still trying").toMatch(
      /disconnected/i,
    );
    expect(
      facts(wrapper),
      "a countdown to a retry of a connection the shell had closed",
    ).not.toHaveProperty("Next try");
  });
});

describe("what it knows about the node", () => {
  it("names the machine it cannot reach", async () => {
    const rig = rigAt();
    rig.connect("pocket");
    const { wrapper } = mountInShell(ConnectionCard, { harness: rig });

    await step(rig, 28_000);

    expect(fact(wrapper, "Node"), "the card forgot which node this shell belongs to").toBe(
      "pocket",
    );
  });

  it("does not invent a name for a node it has never spoken to", async () => {
    const rig = rigAt();
    rig.shell.bus.connect();
    rig.socket().close();
    const { wrapper } = mountInShell(ConnectionCard, { harness: rig });
    await nextTick();

    expect(
      facts(wrapper),
      "the card claimed to know a machine it has never heard from",
    ).not.toHaveProperty("Node");
    expect(wrapper.text(), "an unnamed node was referred to as if it had a name").toMatch(
      /the shell node/i,
    );
  });

  it("counts the seconds of silence, and keeps counting", async () => {
    const rig = rigAt();
    rig.connect("pocket");
    const { wrapper } = mountInShell(ConnectionCard, { harness: rig });

    await step(rig, 28_000); // the node goes quiet; the bus gives up on it

    const shown = fact(wrapper, "Silent for");
    expect(shown, "the silence was not reported in seconds").toMatch(/^\d+s$/);
    // Not pinned to an exact number — the promise is that it reflects the real
    // silence. The bus will not declare a node dead inside 26s, so anything
    // under that cannot be a reading of this connection.
    const first = Number.parseInt(shown, 10);
    expect(first, "the seconds shown are not the seconds that passed").toBeGreaterThanOrEqual(26);

    await step(rig, 5_000);
    const later = Number.parseInt(fact(wrapper, "Silent for"), 10);

    expect(later - first, "the counter froze while the silence went on").toBeGreaterThanOrEqual(4);
  });

  it("does not dress up two seconds of quiet as a symptom", async () => {
    const rig = rigAt();
    rig.connect("pocket");
    const { wrapper } = mountInShell(ConnectionCard, { harness: rig });

    await step(rig, 2_000);
    rig.socket().close(); // this socket does report that it is gone
    await nextTick();

    expect(wrapper.find('[role="alert"]').exists(), "nothing was shown at all").toBe(true);
    expect(
      facts(wrapper),
      "a two second gap was reported as the node being silent",
    ).not.toHaveProperty("Silent for");
  });

  it("repeats the bus's own reason, so a dead node reads differently from a refused one", async () => {
    const rig = rigAt();
    rig.connect("pocket");
    const { wrapper } = mountInShell(ConnectionCard, { harness: rig });

    await step(rig, 28_000);

    const reason = rig.shell.bus.lastError.value;
    if (reason === null) throw new Error("the bus gave up on the node without recording why");
    expect(wrapper.text(), "what the bus knows never reached the operator").toContain(reason);
  });
});

describe("the next attempt", () => {
  it("counts down, so waiting can be told apart from hanging", async () => {
    const rig = rigAt();
    const { wrapper } = mountInShell(ConnectionCard, { harness: rig });
    await stuckInBackoff(rig);

    const shown = fact(wrapper, "Next try");
    expect(shown, "the next attempt was not given as seconds").toMatch(/^\d+s$/);
    const first = Number.parseInt(shown, 10);
    expect(first, "a countdown that starts at zero tells the operator nothing").toBeGreaterThan(0);

    await step(rig, 3_000);
    const later = Number.parseInt(fact(wrapper, "Next try"), 10);

    expect(first - later, "the countdown did not move with the clock").toBe(3);
  });

  it("rounds the wait up, so an attempt that has not happened is not called 'now'", async () => {
    const rig = rigAt();
    const { wrapper } = mountInShell(ConnectionCard, { harness: rig });

    // The first retry is a quarter of a second out. Rounding that down reads
    // "now" for an attempt nothing has made yet, and the operator watching
    // "now" sit there cannot tell a wait from a hang — which is the one thing
    // this countdown exists to tell them.
    rig.shell.bus.connect();
    rig.socket().close();
    await nextTick();

    expect(fact(wrapper, "Next try"), "an attempt still to come was announced as happening").toBe(
      "1s",
    );
  });

  it("never counts down past zero when the attempt is late", async () => {
    const rig = rigAt();
    const { wrapper } = mountInShell(ConnectionCard, { harness: rig });
    await stuckInBackoff(rig);

    // Only the card's own clock moves here: the bus's retry is late, which is
    // what a throttled background tab does to a timer. The operator must not
    // be shown a negative number of seconds until something overdue.
    vi.advanceTimersByTime(15_000);
    await nextTick();

    expect(fact(wrapper, "Next try"), "the countdown ran into negative seconds").toBe("now");
  });

  it("can be had immediately, instead of waiting the backoff out", async () => {
    const rig = rigAt();
    const { wrapper } = mountInShell(ConnectionCard, { harness: rig });
    await stuckInBackoff(rig);
    expect(
      fact(wrapper, "Next try"),
      "this test needs a backoff long enough to be worth skipping",
    ).not.toBe("now");

    const before = rig.sockets.length;
    await tryNow(wrapper).trigger("click");
    await nextTick();

    // No clock is advanced, which is the whole promise: the attempt happens
    // because the operator asked, not because the wait expired.
    expect(rig.sockets.length, "Try now did not attempt a connection").toBe(before + 1);
    expect(
      facts(wrapper),
      "a countdown was still being shown for an attempt already under way",
    ).not.toHaveProperty("Next try");
  });

  it("starts the wait over from the bottom when the attempt it made fails too", async () => {
    const rig = rigAt();
    const { wrapper } = mountInShell(ConnectionCard, { harness: rig });
    await stuckInBackoff(rig);

    const capped = Number.parseInt(fact(wrapper, "Next try"), 10);
    expect(capped, "this test needs a long wait to be worth skipping").toBeGreaterThan(5);

    await tryNow(wrapper).trigger("click");
    rig.socket().close(); // the node is still not there
    await nextTick();

    // Half of what "Try now" promises is that the operator stops waiting. The
    // other half is that asking counts for something afterwards: an operator
    // who presses it at the capped wait and is handed the same cap again is
    // pinned there for as long as they keep asking.
    const shown = fact(wrapper, "Next try");
    const seconds = shown === "now" ? 0 : Number.parseInt(shown, 10);
    expect(
      seconds,
      "pressing Try now left the operator waiting out the cap all over again",
    ).toBeLessThanOrEqual(1);
  });
});

describe("the shell it belongs to", () => {
  it("shows the card without anyone having to ask for it", async () => {
    // The defect this exists for: a component built, unit tested and then
    // never constructed. Every test above would stay green while an operator
    // with a dead node saw a silent, dead-looking workspace instead.
    const rig = rigAt();
    const { wrapper } = mountInShell(App, { harness: rig, props: { shell: rig.shell } });
    await nextTick();

    const alert = wrapper.find('[role="alert"]');
    expect(
      alert.exists(),
      "the shell rendered no connection card while it had no node",
    ).toBe(true);
    expect(alert.text(), "the card is in the shell but tells the operator nothing").toMatch(
      /\S/,
    );
  });
});
