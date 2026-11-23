import { inject, provide, ref, type InjectionKey, type Ref } from "vue";

import { createAppTable, type AppTable } from "../stores/apps.ts";
import { createBus, browserBusEnv, type Bus } from "../stores/bus.ts";
import { createCommands, type CommandsStore } from "../stores/commands.ts";
import { createUi, browserUiEnv, type UiStore } from "../stores/ui.ts";
import {
  closeOverlaysOnLoss,
  createDeviceLabel,
  createSeat,
  followWindows,
  refineFromClientHints,
  type SeatStore,
} from "../stores/seat.ts";
import {
  browserWindowsEnv,
  createWindows,
  type WindowsStore,
} from "../stores/windows.ts";
import {
  browserThemeEnv,
  createThemeController,
  installTokenStylesheet,
  safeStorage,
  type ThemeController,
} from "../tokens/index.ts";
import {
  browserKeyboardEnv,
  createKeyboard,
  DEFAULT_SHORTCUTS,
  type Keyboard,
} from "./keyboard.ts";
import { bindApps, bindConfirmations, shellCommands } from "./shell-commands.ts";
import { browserViewportEnv, createViewport, type Viewport } from "./viewport.ts";
import { createQuickLook, openAppVia, type QuickLookStore } from "./quicklook.ts";
import { createFrameBridge, type FrameBridge } from "./frame-bridge.ts";
import { browserUiChannelEnv, createUiChannel, type UiChannel } from "../uiproto/channel.ts";

/**
 * The composition root.
 *
 * Every store takes its dependencies as arguments and nothing reaches for a
 * global, so this is the only place that knows it is running in a browser.
 * That is the same discipline `lock.rs` applies on the server side, and for
 * the same reason: a shell that reads `window` from the inside out cannot be
 * driven, inspected or embedded anywhere else.
 */
export interface Shell {
  bus: Bus;
  windows: WindowsStore;
  commands: CommandsStore;
  ui: UiStore;
  theme: ThemeController;
  viewport: Viewport;
  keyboard: Keyboard;
  /**
   * Quick Look. In the shell rather than an app because the browser already
   * decodes what it shows, and because it is reachable from Files, from the
   * palette and from any app -- which an app-owned window is not.
   */
  quicklook: QuickLookStore;
  /** Pending Inbox approvals, from the server's `inbox` event. */
  inboxPending: Ref<number>;
  /** Built-in apps merged with what this node has installed. */
  apps: AppTable;
  /** Requests from app frames for shell-only things, such as Quick Look. */
  frames: FrameBridge;
  /** Whether this connection is the one using the workspace. */
  seat: SeatStore;
  /** Windows apps describe over the UI protocol, and their channel. */
  uiproto: UiChannel;
  dispose(): void;
}

export const ShellKey: InjectionKey<Shell> = Symbol("deckjack.shell");

export function createShell(): Shell {
  const storage = safeStorage();
  const theme = createThemeController(browserThemeEnv());
  installTokenStylesheet(document);

  // The device's name rides on every hello, so it is read before the bus
  // connects and from the same storage the operator renames it in.
  const device = createDeviceLabel(storage, navigator.userAgent, {
    touchPoints: navigator.maxTouchPoints,
  });
  refineFromClientHints(device, navigator);
  const bus = createBus({ ...browserBusEnv(), device: () => device.label.value });
  const ui = createUi(browserUiEnv());
  const seat = createSeat({ bus, ui, device });
  const apps = createAppTable();
  const windows = createWindows(bus, browserWindowsEnv(), apps);
  const commands = createCommands({ storage });
  const viewport = createViewport(browserViewportEnv());
  const keyboard = createKeyboard(browserKeyboardEnv(storage));
  // `navigator.onLine` is only trusted when it says false, the same rule the
  // bus follows: a captive portal reports true.
  const quicklook = createQuickLook({
    online: () => navigator.onLine !== false,
    apps,
    // "Open fully" is the shell opening an app, so it goes through the
    // shell's own window store, on the file when the store can carry one.
    openApp: openAppVia(windows),
  });
  const inboxPending = ref(0);

  commands.register(
    shellCommands({
      windows,
      ui,
      bus,
      theme,
      viewport,
      keyboard,
      quicklook,
      // `assign`, not `replace`: the shell is where the operator came from and
      // the back button should take them back to it.
      navigate: (url) => window.location.assign(url),
    }),
  );
  keyboard.bind(DEFAULT_SHORTCUTS);

  /**
   * The server is the source of truth for windows and for which commands
   * exist. Both arrive as events rather than being fetched, because a second
   * client opening a window has to show up here without a poll.
   */
  const offBus = bus.on((event) => {
    switch (event.kind) {
      case "commands":
        commands.adopt(event.commands, async (id, node) => {
          const outcome = await bus.call(id, {}, node);
          if (outcome.status === "queued") {
            ui.notify("info", "Waiting for approval in the Assistant's Inbox.", id);
          } else if (outcome.status !== "ok") {
            ui.notify("error", outcome.message, id);
          }
        });
        break;
      case "inbox":
        inboxPending.value = event.pending;
        break;
      case "lockdown":
        if (event.active) {
          ui.notify("error", "This workspace is locked down. Unlock it on the host.");
        }
        break;
      // Windows are followed by `followWindows`, below, which knows whether
      // this connection holds the seat.
      case "windows":
      case "node":
      case "seat":
      // UI protocol frames are the channel's, below.
      case "ui":
      // Progress is no longer an event: it is correlated to a call and
      // delivered to that call's own handler, so a stray one cannot be
      // attributed to the wrong command.
      case "other":
        break;
    }
  });

  bindConfirmations({ bus, ui });
  const unfollowWindows = followWindows({ bus, windows, seat });
  const stopClosingOverlays = closeOverlaysOnLoss({ seat, ui, quicklook });

  bus.connect();

  // What is installed on this node, loaded when the node says hello, and
  // the palette's "Open" entries kept in step with it. See `bindApps`.
  const unbindApps = bindApps({ bus, commands, windows, apps });

  // App frames ask for Quick Look by posting to this window. See
  // `frame-bridge.ts` for what is accepted and why it is a contract and not a
  // boundary.
  const frames = createFrameBridge({ commands, origin: window.location.origin, apps });
  const onFrameMessage = (event: MessageEvent) => frames.handle(event);
  window.addEventListener("message", onFrameMessage);

  const uiproto = createUiChannel({
    bus,
    windowIds: () => windows.all.value.map((w) => w.id),
    env: browserUiChannelEnv(),
  });

  return {
    bus,
    windows,
    commands,
    ui,
    theme,
    viewport,
    keyboard,
    quicklook,
    inboxPending,
    apps,
    frames,
    seat,
    uiproto,
    dispose() {
      window.removeEventListener("message", onFrameMessage);
      uiproto.dispose();
      unbindApps();
      unfollowWindows();
      stopClosingOverlays();
      seat.dispose();
      offBus();
      viewport.stop();
      bus.close();
    },
  };
}

export function provideShell(shell: Shell): void {
  provide(ShellKey, shell);
}

export function useShell(): Shell {
  const shell = inject(ShellKey);
  if (!shell) {
    // Every component below `App.vue` has it, so a miss means a component was
    // mounted outside the shell — worth failing loudly rather than rebuilding
    // a second set of stores behind the operator's back.
    throw new Error("useShell() outside the Deckjack shell: no stores were provided");
  }
  return shell;
}
