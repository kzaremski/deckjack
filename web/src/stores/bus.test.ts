import { describe, expect, it } from "vitest";

import { createBus, type BusEnv, type SocketLike } from "./bus.ts";
import type { CommandOutcome } from "../protocol.ts";

/**
 * What the shell does when the node is there, and when it is not.
 *
 * These drive the real `createBus` through its injected environment and assert
 * on what a caller observes: whether a command ran, what the operator is told,
 * and whether the shell notices a node that has gone quiet. Nothing here
 * reaches into the bus's internals — no counting of sockets opened, no
 * asserting that some private field moved — because a test written against the
 * mechanism goes green for a rewrite that breaks the promise, which is the
 * failure mode worth designing against.
 *
 * The one unavoidable mechanical detail is the frames on the wire. Those are
 * the protocol, not an implementation choice: the node will not answer a probe
 * the shell did not send.
 */

/** A socket the test drives: nothing happens until the test says so. */
class FakeSocket implements SocketLike {
  sent: string[] = [];
  closed = false;
  /** Set false to model a socket so dead it never reports closing. */
  firesOnClose = true;

  onopen: ((this: unknown, ev: unknown) => void) | null = null;
  onclose: ((this: unknown, ev: unknown) => void) | null = null;
  onerror: ((this: unknown, ev: unknown) => void) | null = null;
  onmessage: ((this: unknown, ev: { data: unknown }) => void) | null = null;

  send(data: string) {
    this.sent.push(data);
  }

  close() {
    this.closed = true;
    if (this.firesOnClose) this.onclose?.call(this, {});
  }

  /** The handshake the node answers with. */
  open() {
    this.onopen?.call(this, {});
    this.deliver({ type: "welcome", protocol: 1, connection: 1, node_id: "n", host_name: "pocket" });
  }

  deliver(frame: unknown) {
    this.onmessage?.call(this, { data: JSON.stringify(frame) });
  }

  /** Frames the shell sent, decoded. */
  frames(): Array<Record<string, unknown>> {
    return this.sent.map((s) => JSON.parse(s) as Record<string, unknown>);
  }

  pings(): Array<Record<string, unknown>> {
    return this.frames().filter((f) => f.type === "ping");
  }

  /** Answer the most recent ping the way a live node would. */
  pong() {
    const last = this.pings().at(-1);
    this.deliver({ type: "pong", token: last?.token });
  }
}

interface Rig {
  bus: ReturnType<typeof createBus>;
  sockets: FakeSocket[];
  socket(): FakeSocket;
  /** Move the clock and run whatever came due. */
  advance(ms: number): void;
  setOnline(up: boolean): void;
}

function rig(options: { online?: boolean } = {}): Rig {
  const sockets: FakeSocket[] = [];
  let clock = 1_000;
  let online = options.online ?? true;
  let notifyOnline: ((up: boolean) => void) | null = null;
  let timers: Array<{ at: number; fn: () => void; dead: boolean }> = [];

  const env: BusEnv = {
    url: "wss://pocket.example.ts.net/ws",
    connect() {
      const s = new FakeSocket();
      sockets.push(s);
      return s;
    },
    now: () => clock,
    delay(ms, fn) {
      const entry = { at: clock + ms, fn, dead: false };
      timers.push(entry);
      return () => {
        entry.dead = true;
      };
    },
    online: () => online,
    watchOnline(fn) {
      notifyOnline = fn;
      return () => {
        notifyOnline = null;
      };
    },
  };

  const bus = createBus(env);

  return {
    bus,
    sockets,
    socket: () => sockets.at(-1)!,
    advance(ms) {
      const target = clock + ms;
      // Run in due order, because a task may schedule another.
      for (;;) {
        const next = timers
          .filter((t) => !t.dead && t.at <= target)
          .sort((a, b) => a.at - b.at)[0];
        if (!next) break;
        clock = Math.max(clock, next.at);
        next.dead = true;
        timers = timers.filter((t) => !t.dead);
        next.fn();
      }
      clock = target;
    },
    setOnline(up) {
      online = up;
      notifyOnline?.(up);
    },
  };
}

/** A connected bus with the handshake done. */
function connected(options: { online?: boolean } = {}): Rig {
  const r = rig(options);
  r.bus.connect();
  r.socket().open();
  return r;
}

describe("a command the node never received", () => {
  it("is refused rather than reported as done", async () => {
    const r = rig();
    r.bus.connect(); // socket exists but has not opened

    const outcome = await r.bus.call("shell.window.open", { app: "terminal" });

    expect(outcome.status).not.toBe("ok");
  });

  it("refuses window commands too, because the node is what holds the windows", async () => {
    // The regression this exists for: the bus used to resolve anything named
    // `shell.*` as ok while disconnected, so every window the operator opened,
    // moved or closed reported success to a node that never heard about it.
    const r = rig();
    r.bus.connect();

    for (const command of [
      "shell.window.open",
      "shell.window.close",
      "shell.window.place",
      "shell.windows.tile",
      "shell.split.set",
    ]) {
      const outcome = await r.bus.call(command, {});
      expect(outcome.status, `${command} was reported as done`).not.toBe("ok");
    }
  });

  it("says the command did not run", async () => {
    const r = rig();
    r.bus.connect();

    const outcome = await r.bus.call("shell.window.open", {});

    expect(outcome.status).toBe("error");
    if (outcome.status !== "error") return;
    expect(outcome.message).toMatch(/not run/i);
  });

  it("tells the operator which thing to go and fix", async () => {
    // A dead wifi and a dead node are different errands. Saying the wrong one
    // sends someone to check a router when their workspace machine is down.
    const offline = rig({ online: false });
    offline.bus.connect();
    const a = await offline.bus.call("shell.window.open", {});

    const unreachable = rig({ online: true });
    unreachable.bus.connect();
    const b = await unreachable.bus.call("shell.window.open", {});

    expect(a.status === "error" && a.message).toMatch(/offline/i);
    expect(b.status === "error" && b.message).not.toMatch(/offline/i);
    expect(b.status === "error" && b.message).toMatch(/pocket\.example\.ts\.net/);
  });
});

describe("a connected node", () => {
  it("accepts commands", async () => {
    const r = connected();
    const call = r.bus.call("shell.window.open", { app: "terminal" });

    const invoke = r.socket().frames().find((f) => f.type === "invoke");
    expect(invoke, "nothing was sent to the node").toBeTruthy();
    r.socket().deliver({
      type: "response",
      id: invoke!.id,
      outcome: { status: "ok", value: null },
      elapsed_ms: 1,
    });

    expect((await call).status).toBe("ok");
  });

  it("is proved alive by a round trip, not by the socket looking open", () => {
    const r = connected();
    expect(r.bus.rtt.value).toBeNull();

    r.advance(8_000);
    expect(r.socket().pings().length, "the shell never probed the node").toBe(1);

    r.advance(40);
    r.socket().pong();

    expect(r.bus.rtt.value).not.toBeNull();
    expect(r.bus.status.value).toBe("open");
  });

  it("survives one unanswered probe, because a phone changing cells misses one", () => {
    const r = connected();

    r.advance(8_000); // probe 1, unanswered
    r.advance(8_000); // probe 2, unanswered

    expect(r.bus.status.value).toBe("open");
  });
});

describe("a node that stops answering", () => {
  it("is noticed, even though the socket never said anything was wrong", () => {
    // Both ends are pinned deliberately. A test that only checked the far end
    // could be satisfied by tearing the connection down at the first missed
    // probe, which would drop a working session every time a phone changed
    // cells; one that only checked the near end could be satisfied by never
    // noticing at all. The promise is a window, so the test asserts a window.
    const r = connected();

    r.advance(20_000);
    expect(r.bus.status.value, "a slow link was torn down too eagerly").toBe("open");

    r.advance(8_000); // past the silence budget
    expect(r.bus.status.value, "a dead node went unnoticed").not.toBe("open");
  });

  it("answers every call that was still in flight", async () => {
    // Otherwise the caller awaits forever: the UI shows a command running
    // against a node that is gone and never resolves.
    const r = connected();
    const socket = r.socket();
    // A socket this dead never reports closing, which is the whole situation:
    // if it could tell us it had closed, the heartbeat would not be needed.
    socket.firesOnClose = false;

    const inFlight = r.bus.call("files.read", { path: "/x" });
    r.advance(28_000);

    const outcome = (await Promise.race([
      inFlight,
      new Promise<CommandOutcome>((resolve) =>
        setTimeout(() => resolve({ id: "timeout", status: "error", message: "HUNG" }), 20),
      ),
    ])) as CommandOutcome;

    expect(outcome.status === "error" && outcome.message).not.toBe("HUNG");
  });

  it("tries again soon, rather than waiting out a backoff it never earned", () => {
    // A connection that has died once has not demonstrated that the node is
    // unreachable for long, so the first retry should be prompt. Making the
    // operator wait is the shell being pessimistic on evidence it does not
    // have.
    //
    // Stepped rather than advanced in one jump, and the count is taken *after*
    // it gives up. An earlier version of this test advanced far past the
    // deadline in a single step, so the retry had already happened inside that
    // step and the assertion held no matter how long the backoff was — it
    // survived a mutation that doubled the backoff, which is exactly the bug
    // it was written for.
    const r = connected();
    for (let i = 0; i < 200 && r.bus.status.value === "open"; i++) r.advance(250);
    expect(r.bus.status.value, "the node was never given up on").not.toBe("open");

    const before = r.sockets.length;
    r.advance(250);

    expect(r.sockets.length, "the first retry was pushed out").toBeGreaterThan(before);
  });
});

describe("the network coming back", () => {
  it("is not waited out", () => {
    const r = rig({ online: false });
    r.bus.connect();
    r.socket().open();
    r.advance(28_000); // dies while offline
    const before = r.sockets.length;

    r.setOnline(true);

    expect(r.sockets.length, "the shell sat on its backoff after the network returned")
      .toBeGreaterThan(before);
  });
});
