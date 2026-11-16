/**
 * A real shell, driven by the test rather than by a browser.
 *
 * Component tests need a `Shell` to inject, and the tempting shortcut is a
 * bag of stub refs. That shortcut is why the bus's "every `shell.*` call
 * succeeds" lie survived so long: a stub agrees with whatever the component
 * expects, so the test passes either way and proves nothing.
 *
 * So this builds the **real** stores — `createBus`, `createWindows`,
 * `createUi`, the rest — and fakes only the edges of the world: the socket,
 * the clock, storage and `matchMedia`. A component mounted on this is talking
 * to the same state machines that run in production, and the test drives the
 * node's behavior instead of describing it.
 *
 * What is NOT injectable, and so is not faked here: `Date.now()` and
 * `setInterval`, which `ConnectionCard` and `TopBar` call directly for their
 * own ticking. Use `vi.useFakeTimers({ now })` in tests that care, and
 * remember `advance()` below moves the *bus's* clock, which is separate.
 */
import { provide } from "vue";
import { mount, type VueWrapper } from "@vue/test-utils";

import { ShellKey, type Shell } from "../shell/context.ts";
import { createBus, type BusEnv, type SocketLike } from "../stores/bus.ts";
import { createCommands } from "../stores/commands.ts";
import { createUi } from "../stores/ui.ts";
import { createWindows } from "../stores/windows.ts";
import { createThemeController } from "../tokens/index.ts";
import { createKeyboard, DEFAULT_SHORTCUTS } from "../shell/keyboard.ts";
import { createViewport } from "../shell/viewport.ts";
import { shellCommands } from "../shell/shell-commands.ts";
import { ref } from "vue";

/** A socket the test drives: nothing happens until the test says so. */
export class FakeSocket implements SocketLike {
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

  /** The handshake a live node answers with. */
  open(hostName = "pocket") {
    this.onopen?.call(this, {});
    this.deliver({
      type: "welcome",
      protocol: 1,
      connection: 1,
      node_id: "n",
      host_name: hostName,
    });
  }

  deliver(frame: unknown) {
    this.onmessage?.call(this, { data: JSON.stringify(frame) });
  }

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

function memoryStorage() {
  const map = new Map<string, string>();
  return {
    read: (k: string) => map.get(k) ?? null,
    write: (k: string, v: string) => void map.set(k, v),
  };
}

/** A `MediaQueryList` that never matches and never changes, unless told to. */
function stubMedia(matches: (query: string) => boolean) {
  return (query: string) =>
    ({
      matches: matches(query),
      media: query,
      onchange: null,
      addEventListener: () => {},
      removeEventListener: () => {},
      addListener: () => {},
      removeListener: () => {},
      dispatchEvent: () => false,
    }) as unknown as MediaQueryList;
}

export interface HarnessOptions {
  /** Whether the device has a network. Default true. */
  online?: boolean;
  /** Viewport size the viewport store reports. Default a desktop. */
  size?: { w: number; h: number };
  /** True for a touch device, which flips the shell to its compact mode. */
  coarsePointer?: boolean;
}

export interface Harness {
  shell: Shell;
  /** Every socket the bus has opened, newest last. */
  sockets: FakeSocket[];
  socket(): FakeSocket;
  /** Move the bus's clock and run whatever came due. */
  advance(ms: number): void;
  /** Flip `navigator.onLine` and notify the bus, as the browser would. */
  setOnline(up: boolean): void;
  /** Connect and complete the handshake, so the shell has a live node. */
  connect(hostName?: string): void;
  dispose(): void;
}

/**
 * Builds the shell. Nothing is connected yet: call `connect()` for a live
 * node, or leave it alone to test what the shell says when there is none.
 */
export function harness(options: HarnessOptions = {}): Harness {
  const sockets: FakeSocket[] = [];
  let clock = 1_000;
  let online = options.online ?? true;
  let notifyOnline: ((up: boolean) => void) | null = null;
  let timers: Array<{ at: number; fn: () => void; dead: boolean }> = [];
  const size = options.size ?? { w: 1600, h: 900 };
  const storage = memoryStorage();

  const busEnv: BusEnv = {
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

  const bus = createBus(busEnv);
  const ui = createUi({
    delay(ms, fn) {
      const entry = { at: clock + ms, fn, dead: false };
      timers.push(entry);
      return () => {
        entry.dead = true;
      };
    },
  });
  const windows = createWindows(bus, { id: idFactory(), storage });
  const commands = createCommands({ storage });
  const theme = createThemeController({
    root: document.documentElement,
    document,
    storage,
    media: stubMedia(() => false),
  });
  const viewport = createViewport({
    media: stubMedia((q) =>
      q.includes("pointer: coarse") ? (options.coarsePointer ?? false) : false,
    ),
    size: () => size,
    onResize: () => () => {},
    hingeFraction: () => null,
  });
  const keyboard = createKeyboard({ apple: false, storage });
  const inboxPending = ref(0);

  commands.register(shellCommands({ windows, ui, bus, theme, viewport, keyboard }));
  keyboard.bind(DEFAULT_SHORTCUTS);

  const shell: Shell = {
    bus,
    windows,
    commands,
    ui,
    theme,
    viewport,
    keyboard,
    inboxPending,
    dispose() {
      viewport.stop();
      bus.close();
    },
  };

  function advance(ms: number) {
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
  }

  return {
    shell,
    sockets,
    socket: () => sockets.at(-1)!,
    advance,
    setOnline(up) {
      online = up;
      notifyOnline?.(up);
    },
    connect(hostName) {
      bus.connect();
      sockets.at(-1)!.open(hostName);
    },
    dispose() {
      shell.dispose();
    },
  };
}

function idFactory() {
  let n = 0;
  return () => `w${++n}`;
}

/**
 * Mounts a component with the shell provided, the way `App.vue` does.
 *
 * Returns the wrapper and the harness together, because a component test
 * almost always has to drive the node to say anything interesting.
 */
export function mountInShell(
  component: unknown,
  options: Record<string, unknown> & { harness?: Harness } = {},
): { wrapper: VueWrapper; rig: Harness } {
  const { harness: provided, ...mountOptions } = options;
  const rig = provided ?? harness();
  const global = (mountOptions.global ?? {}) as Record<string, unknown>;
  const provide = (global.provide ?? {}) as Record<string | symbol, unknown>;

  // One cast, because @vue/test-utils' generics cannot follow a wrapper that
  // takes any component. The alternative is making every caller restate them.
  const mountLoosely = mount as unknown as (
    c: unknown,
    o?: Record<string, unknown>,
  ) => VueWrapper;

  const wrapper = mountLoosely(component, {
    ...mountOptions,
    global: {
      ...global,
      provide: { ...provide, [ShellKey as unknown as symbol]: rig.shell },
    },
  });
  return { wrapper, rig };
}

/** The provider form, for a component that must be mounted inside a parent. */
export function shellProvider(rig: Harness) {
  return {
    setup(_: unknown, { slots }: { slots: Record<string, () => unknown> }) {
      provide(ShellKey, rig.shell);
      return () => slots.default?.();
    },
  };
}
