import { inject, provide, ref, type InjectionKey, type Ref } from "vue";

import { createBus, browserBusEnv, type Bus } from "../stores/bus.ts";
import type { Risk } from "../protocol.ts";
import { createCommands, type CommandsStore } from "../stores/commands.ts";
import { createUi, browserUiEnv, type UiStore } from "../stores/ui.ts";
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
import { shellCommands } from "./shell-commands.ts";
import { browserViewportEnv, createViewport, type Viewport } from "./viewport.ts";

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
  /** Pending Inbox approvals, from the server's `inbox` event. */
  inboxPending: Ref<number>;
  dispose(): void;
}

export const ShellKey: InjectionKey<Shell> = Symbol("deckjack.shell");

export function createShell(): Shell {
  const storage = safeStorage();
  const theme = createThemeController(browserThemeEnv());
  installTokenStylesheet(document);

  const bus = createBus(browserBusEnv());
  const ui = createUi(browserUiEnv());
  const windows = createWindows(bus, browserWindowsEnv());
  const commands = createCommands({ storage });
  const viewport = createViewport(browserViewportEnv());
  const keyboard = createKeyboard(browserKeyboardEnv(storage));
  const inboxPending = ref(0);

  commands.register(
    shellCommands({ windows, ui, bus, theme, viewport, keyboard }),
  );
  keyboard.bind(DEFAULT_SHORTCUTS);

  /**
   * The server is the source of truth for windows and for which commands
   * exist. Both arrive as events rather than being fetched, because a second
   * client opening a window has to show up here without a poll.
   */
  const offBus = bus.on((event) => {
    switch (event.kind) {
      case "windows":
        windows.adopt(event.windows);
        break;
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
      case "node":
      // Progress is no longer an event: it is correlated to a call and
      // delivered to that call's own handler, so a stray one cannot be
      // attributed to the wrong command.
      case "other":
        break;
    }
  });

  // The bus asks the operator through the shell's own dialog, using the
  // prompt the backend wrote rather than wording invented here.
  bus.onConfirm(async ({ command, prompt, risk }) =>
    ui.confirm({
      command,
      title: "Confirm",
      detail: prompt,
      risk: risk as Risk,
    }),
  );

  bus.connect();

  return {
    bus,
    windows,
    commands,
    ui,
    theme,
    viewport,
    keyboard,
    inboxPending,
    dispose() {
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
