import { watch } from "vue";

import { awaitsApproval, type AppContract, type AppTable, type InstalledApp } from "../stores/apps.ts";
import type { CommandEntry, CommandsStore } from "../stores/commands.ts";
import type { UiStore } from "../stores/ui.ts";
import type { SeatStore } from "../stores/seat.ts";
import type { WindowsStore } from "../stores/windows.ts";
import type { Bus } from "../stores/bus.ts";
import type { Risk } from "../protocol.ts";
import { themeList, type DensityName, type ThemeController } from "../tokens/index.ts";
import type { Keyboard } from "./keyboard.ts";
import type { Viewport } from "./viewport.ts";
import type { QuickLookStore } from "./quicklook.ts";
import { APP_OPEN, parseAppOpen } from "./open-app.ts";

/**
 * The shell's own commands.
 *
 * These are the ones whose backend is the shell itself: opening a window,
 * snapping it, switching layout, theme and density. They are registered in
 * the same registry as everything an app or adapter provides, with the same
 * fields, because the palette, the keyboard and an agent must not be able to
 * tell the difference.
 *
 * Risk is set here for the same reason a backend sets it elsewhere: the UI
 * does not get to infer it. Window arrangement is a `write` because it
 * changes server state; `system.lockdown` is `destructive` and goes through a
 * confirmation even for the operator.
 */
export interface ShellCommandDeps {
  windows: WindowsStore;
  ui: UiStore;
  bus: Bus;
  theme: ThemeController;
  viewport: Viewport;
  keyboard: Keyboard;
  quicklook: QuickLookStore;
  /**
   * Leaves the shell for one of the node's own pages.
   *
   * Injected rather than calling `window.location` from inside a command, for
   * the same reason every store here takes its dependencies as arguments: a
   * command that reaches for a global cannot be driven or tested, and "did
   * this actually go anywhere" is the only thing worth asserting about a
   * command whose entire job is to go somewhere.
   */
  navigate(url: string): void;
}

export function shellCommands(deps: ShellCommandDeps): CommandEntry[] {
  const { windows, ui, bus, theme, viewport, keyboard, quicklook, navigate } = deps;

  const focused = () => windows.focused.value;
  const hasWindow = () => focused() !== null;

  const entries: CommandEntry[] = [
    {
      /**
       * The gap this closes: every route for enrolling a passkey has been
       * mounted and working for as long as `passkeyhttp` has existed, and
       * nothing in the product called any of them. Registering a credential
       * meant pasting a script into a devtools console, which rules out every
       * phone -- and the sign-in page could not lead to having anything to
       * sign in with.
       *
       * A command rather than a settings screen: the palette is where every
       * other thing the shell can do already lives, and one link does not
       * earn a screen of its own.
       */
      id: "shell.passkey.enroll",
      title: "Set up a passkey",
      summary: "Enroll this device, or add another one",
      risk: "read_only",
      group: "Shell",
      keywords: ["passkey", "webauthn", "enroll", "device", "sign in", "security key"],
      run: () => {
        navigate("/passkey/enroll");
      },
    },
    {
      /**
       * Quick Look, from the palette. `path` is a parameter rather than a
       * selection read out of the Files panel, because a command has to be
       * callable by an agent and from the CLI -- principle 3 -- and neither
       * of those has a selection.
       */
      id: "shell.quicklook.open",
      title: "Quick Look",
      summary: "Preview a file without opening its app",
      risk: "read_only",
      group: "Shell",
      keywords: ["preview", "quick look", "peek", "view", "open"],
      run: (params) => {
        const path = typeof params?.path === "string" ? params.path : null;
        if (!path) {
          ui.notify("error", "Quick Look needs a file path.", "shell.quicklook.open");
          return;
        }
        // The folder to step through, from whoever knows it -- Files sends
        // its listing. Only strings reach the store; a list that does not
        // contain the file is the store's to refuse, and it does (no
        // position, no stepping), so that rule lives in one place.
        const siblings = Array.isArray(params?.siblings)
          ? params.siblings.filter((s): s is string => typeof s === "string")
          : undefined;
        quicklook.open(path, siblings);
      },
    },
    {
      id: "shell.palette.open",
      title: "Command palette",
      summary: "Every command on the bus, from one field",
      risk: "read_only",
      group: "Shell",
      keywords: ["palette", "run", "search", "commands"],
      run: () => {
        ui.paletteOpen.value = true;
      },
    },
    {
      id: "shell.switcher.open",
      title: "Switch window",
      summary: "Open windows, most recently used first",
      risk: "read_only",
      group: "Windows",
      keywords: ["switcher", "alt tab", "recent"],
      run: () => {
        ui.switcherOpen.value = true;
      },
    },
    {
      id: "shell.window.close",
      title: "Close window",
      risk: "write",
      group: "Windows",
      available: hasWindow,
      run: async () => {
        const w = focused();
        if (w) await windows.close(w.id);
      },
    },
    {
      id: "shell.window.minimize",
      title: "Minimize window",
      risk: "write",
      group: "Windows",
      available: hasWindow,
      run: () => {
        const w = focused();
        if (w) windows.minimize(w.id);
      },
    },
    {
      id: "shell.window.maximize",
      title: "Maximize window",
      risk: "write",
      group: "Windows",
      available: hasWindow,
      run: () => {
        const w = focused();
        if (w) windows.toggleMaximize(w.id);
      },
    },
    {
      id: "shell.window.snap.left",
      title: "Snap window left",
      risk: "write",
      group: "Windows",
      keywords: ["tile", "half"],
      available: hasWindow,
      run: async () => {
        const w = focused();
        if (w) await windows.snap(w.id, "left");
      },
    },
    {
      id: "shell.window.snap.right",
      title: "Snap window right",
      risk: "write",
      group: "Windows",
      keywords: ["tile", "half"],
      available: hasWindow,
      run: async () => {
        const w = focused();
        if (w) await windows.snap(w.id, "right");
      },
    },
    {
      id: "shell.window.next",
      title: "Next window",
      risk: "read_only",
      group: "Windows",
      run: () => windows.cycle(1),
    },
    {
      id: "shell.window.previous",
      title: "Previous window",
      risk: "read_only",
      group: "Windows",
      run: () => windows.cycle(-1),
    },
    {
      id: "shell.windows.tile",
      title: "Tile windows side by side",
      summary: "Once, into one row. For a mode that stays on, use the tiling layout",
      risk: "write",
      group: "Windows",
      run: () => windows.tile(),
    },
    {
      id: "shell.layout.toggle",
      title: "Toggle tiling layout",
      summary: "Floating is the default; tiling keeps windows in master-and-stack",
      risk: "write",
      group: "Windows",
      keywords: ["tiling", "floating", "i3", "hyprland", "layout", "master", "stack"],
      run: () => windows.toggleLayout(),
    },
    {
      id: "shell.layout.floating",
      title: "Layout: floating",
      summary: "Windows you drag and snap, each back where it last floated",
      risk: "write",
      group: "Windows",
      keywords: ["floating", "layout", "drag"],
      available: () => windows.layout.value !== "floating",
      run: () => windows.setLayout("floating"),
    },
    {
      id: "shell.layout.tiling",
      title: "Layout: tiling",
      summary: "Master and stack, kept arranged as windows open and close",
      risk: "write",
      group: "Windows",
      keywords: ["tiling", "layout", "i3", "hyprland", "master", "stack"],
      available: () => windows.layout.value !== "tiling",
      run: () => windows.setLayout("tiling"),
    },
    {
      id: "shell.split.toggle",
      title: "Split with the last window",
      summary: "Compact layout: two windows with a draggable divider",
      risk: "write",
      group: "Windows",
      available: () => windows.recent.value.length >= 2,
      run: () => {
        if (windows.split.value) {
          windows.clearSplit();
          return;
        }
        const [first, second] = windows.recent.value;
        if (first && second) windows.setSplit(first.id, second.id);
      },
    },
    {
      id: "shell.mode.toggle",
      title: "Switch between desktop and compact layout",
      summary: "Overrides what the viewport and pointer imply",
      risk: "write",
      group: "Shell",
      keywords: ["layout", "mobile", "tablet", "desktop"],
      run: () => {
        viewport.override.value =
          viewport.mode.value === "desktop" ? "compact" : "desktop";
      },
    },
    {
      id: "shell.mode.auto",
      title: "Follow the viewport for layout",
      risk: "write",
      group: "Shell",
      available: () => viewport.override.value !== null,
      run: () => {
        viewport.override.value = null;
      },
    },
    {
      id: "shell.theme.cycle",
      title: "Next theme",
      risk: "write",
      group: "Appearance",
      keywords: ["dark", "light", "night", "circuit"],
      run: () => {
        const index = themeList.findIndex((t) => t.name === theme.theme.value);
        const next = themeList[(index + 1) % themeList.length];
        if (next) theme.setTheme(next.name);
      },
    },
    {
      id: "shell.keyboard.lock",
      title: "Capture browser shortcuts",
      summary: "Ctrl/Cmd+W, Ctrl/Cmd+T, Tab and Escape reach Deckjack",
      risk: "write",
      group: "Shell",
      keywords: ["keyboard", "lock", "shortcuts"],
      run: async () => {
        const status = await keyboard.requestLock();
        if (status === "locked") {
          ui.notify("info", "Browser shortcuts now reach Deckjack.");
        } else if (status === "unsupported") {
          ui.notify("warn", "This browser has no Keyboard Lock API; Chromium does.");
        } else {
          ui.notify(
            "warn",
            "The browser refused Keyboard Lock. It is granted to an installed window or in fullscreen.",
          );
        }
      },
    },
    {
      id: "system.lockdown",
      title: "Lock down this workspace",
      summary: "Revokes every session, kills children, freezes outbound actions",
      risk: "destructive",
      group: "Security",
      keywords: ["kill switch", "panic", "revoke"],
      run: async () => {
        const ok = await ui.confirm({
          command: "system.lockdown",
          title: "Lock down Deckjack?",
          detail:
            "Every session is revoked and the signing key rotated. Terminals, agents and app binaries are killed, queued mail and Inbox items freeze, and unlocking needs the passkey plus `deckjack unlock` on the host.",
          risk: "destructive",
        });
        if (!ok) return;
        const outcome = await bus.call("system.lockdown", {});
        if (outcome.status !== "ok") {
          ui.notify(
            "error",
            outcome.status === "error" || outcome.status === "refused"
              ? outcome.message
              : "Lockdown is waiting in the Inbox, which is not where a kill switch belongs.",
            "system.lockdown",
          );
        }
      },
    },
  ];

  for (const name of themeList) {
    entries.push({
      id: `shell.theme.set.${name.name}`,
      title: `Theme: ${name.label}`,
      risk: "write",
      group: "Appearance",
      keywords: ["theme", name.scheme],
      available: () => theme.theme.value !== name.name,
      run: () => theme.setTheme(name.name),
    });
  }

  const densities: DensityName[] = ["compact", "cozy", "comfortable"];
  for (const density of densities) {
    entries.push({
      id: `shell.density.set.${density}`,
      title: `Density: ${density[0]!.toUpperCase()}${density.slice(1)}`,
      summary: "Scales spacing, control height, touch targets and type",
      risk: "write",
      group: "Appearance",
      keywords: ["density", "compact", "touch", "spacing"],
      available: () => theme.density.value !== density,
      run: () => theme.setDensity(density),
    });
  }

  return entries;
}

/** One "Open <app>" palette entry per app the dock offers. */
export function appOpenEntries(dock: AppContract[], windows: WindowsStore): CommandEntry[] {
  return dock.map((app) => ({
    id: `app.open.${app.id}`,
    title: `Open ${app.title}`,
    summary: app.summary,
    risk: "write",
    group: "Apps",
    keywords: [app.id, "app", "window", "open"],
    run: () => windows.open(app.id).then(() => undefined),
  }));
}

/**
 * One "Approve <title>" entry per installed app awaiting the operator's
 * approval: never approved, or approved for a manifest that has since
 * changed (every app approved before the fingerprint became SHA-256 is in
 * this state once). This is the re-approval path the core's refusals and
 * `deckjack install` point at.
 *
 * It runs `system.app.approve`, which is cross-project and so asks the
 * operator to confirm through the bus's own dialog. The summary says what is
 * being granted, so it is read before the entry is chosen. After a recorded
 * approval the installed list is asked for again, which removes the entry.
 */
export function appApproveEntries(
  installed: InstalledApp[],
  bus: Bus,
  reload: () => Promise<void>,
): CommandEntry[] {
  return installed.filter(awaitsApproval).map((app) => ({
    id: `app.approve.${app.name}`,
    title: `Approve ${app.title}`,
    summary:
      (app.approval === "stale"
        ? "Its approval no longer covers what it asks for. "
        : "Not approved yet. ") + `It asks for ${app.asks ?? "permissions"}.`,
    risk: "cross_project",
    group: "Apps",
    keywords: [app.name, "approve", "permissions", "grant", "allow"],
    run: async () => {
      const outcome = await bus.call("system.app.approve", { app: app.name });
      if (outcome.status === "ok") {
        await reload();
        return;
      }
      throw new Error(
        outcome.status === "queued"
          ? `approving ${app.title} is waiting in the Inbox`
          : outcome.message,
      );
    },
  }));
}

/**
 * `shell.app.open`: open an app on a file, the shell's half of the open-app
 * contract. Frames reach it through the frame bridge (`shell.openApp` in
 * `/kit/shell.js`); the palette and an agent reach it as a command with the
 * same parameters, checked by the same `parseAppOpen`.
 */
export function appOpenCommand(windows: WindowsStore, apps: AppTable): CommandEntry {
  return {
    id: APP_OPEN,
    title: "Open in an app",
    summary: "Open a file in an app, or bring that app's window forward",
    risk: "write",
    group: "Apps",
    keywords: ["open with", "open in", "editor", "file", "app"],
    run: async (params) => {
      const parsed = parseAppOpen(params, apps);
      if (!parsed.ok) throw new Error(parsed.reason);
      const { app, path } = parsed.request;
      await windows.openApp(app, path ? { path } : undefined);
    },
  };
}

export interface AppBindingDeps {
  bus: Bus;
  commands: CommandsStore;
  windows: WindowsStore;
  apps: AppTable;
}

/**
 * Keeps the app table and the palette in step with the node.
 *
 * Two jobs, and both are here so `createShell` and the test harness run the
 * same code rather than each assembling its own:
 *
 * - **Load `system.apps` when a node says hello.** Not when the shell is
 *   built: the socket is still connecting then, and `bus.call` refuses
 *   anything sent before it is open, so the list used to come back empty on
 *   every page load and no installed app ever got its own UI. The welcome
 *   frame is also what a reconnect, or a different node taking over, sends,
 *   so the list follows the node instead of the first page load.
 * - **Re-register the "Open" entries whenever the dock changes**, so an app
 *   that was installed is in the palette as soon as it is in the dock.
 *
 * Returns the function that stops both.
 */
export function bindApps(deps: AppBindingDeps): () => void {
  const { bus, commands, windows, apps } = deps;
  let unregister: (() => void) | null = null;
  let unregisterApprove: (() => void) | null = null;
  /**
   * Whether the list has been asked for on this connection and not refused
   * for want of the seat. A device that connects while another holds the
   * seat is refused `system.apps`; it asks again when the seat is its own.
   */
  let asked = false;
  const load = () => {
    asked = true;
    return apps.load((command, params) =>
      bus.call(command, params ?? {}).then((outcome) => {
        if (outcome.status === "ok") return outcome.value;
        if (outcome.status === "error" && outcome.code === "seat_not_held") asked = false;
        return Promise.reject(new Error("refused"));
      }),
    );
  };
  const stopApproveWatch = watch(
    apps.installed,
    (installed) => {
      unregisterApprove?.();
      unregisterApprove = commands.register(appApproveEntries(installed, bus, load));
    },
    { immediate: true },
  );
  const unregisterOpen = commands.register([appOpenCommand(windows, apps)]);
  const stopWatch = watch(
    apps.dock,
    (dock) => {
      unregister?.();
      unregister = commands.register(appOpenEntries(dock, windows));
    },
    { immediate: true },
  );
  const offBus = bus.on((event) => {
    if (event.kind === "node") void load();
    else if (event.kind === "seat" && event.seat.held && !asked) void load();
  });
  return () => {
    stopWatch();
    stopApproveWatch();
    unregisterApprove?.();
    offBus();
    unregister?.();
    unregisterOpen();
  };
}

/**
 * Opens an app once the shell can: the node connected, this device holding
 * the seat, and the app known.
 *
 * For the PWA's `/?open=<app>` shortcuts. Dispatching at page load, as this
 * used to, ran while the socket was still connecting, and opening a window is
 * a record on the node, so the windows store refused with "not connected"
 * and the shortcut did nothing at all. An installed app is also only known
 * once `system.apps` has answered. A name that never becomes known is never
 * opened, and it opens at most once.
 *
 * The seat, because the socket being open is not yet permission to change
 * the workspace: until the node says this connection holds the seat, an open
 * may be refused (another device has it), and one sent before the node's
 * window snapshot arrives is overwritten by that snapshot.
 *
 * Returns a function that gives up waiting.
 */
export function openWhenReady(
  deps: Pick<AppBindingDeps, "bus" | "apps" | "commands"> & { seat: Pick<SeatStore, "held"> },
  app: string,
): () => void {
  let done = false;
  const stop = watch(
    () => deps.bus.status.value === "open" && deps.seat.held.value && deps.apps.isKnown(app),
    (ready) => {
      if (!ready || done) return;
      done = true;
      void deps.commands.dispatch(`app.open.${app}`);
    },
    { immediate: true },
  );
  return () => {
    done = true;
    stop();
  };
}

/**
 * The bus asks the operator through the shell's own dialog, using the prompt
 * and target the node rendered rather than wording invented here.
 *
 * A function rather than inline in `createShell` so the test harness wires
 * the dialog exactly as the product does: a target the node sends that never
 * reaches the dialog is a confirmation the operator answers blind.
 */
export function bindConfirmations({ bus, ui }: { bus: Bus; ui: UiStore }): void {
  bus.onConfirm(async ({ command, prompt, risk, target }) =>
    ui.confirm({
      command,
      title: "Confirm",
      detail: prompt,
      risk: risk as Risk,
      target,
    }),
  );
}
