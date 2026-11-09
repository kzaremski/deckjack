import { apps, dockOrder } from "../stores/apps.ts";
import type { CommandEntry } from "../stores/commands.ts";
import type { UiStore } from "../stores/ui.ts";
import type { WindowsStore } from "../stores/windows.ts";
import type { Bus } from "../stores/bus.ts";
import { themeList, type DensityName, type ThemeController } from "../tokens/index.ts";
import type { Keyboard } from "./keyboard.ts";
import type { Viewport } from "./viewport.ts";

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
}

export function shellCommands(deps: ShellCommandDeps): CommandEntry[] {
  const { windows, ui, bus, theme, viewport, keyboard } = deps;

  const focused = () => windows.focused.value;
  const hasWindow = () => focused() !== null;

  const entries: CommandEntry[] = [
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
      risk: "write",
      group: "Windows",
      run: () => windows.tile(),
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

  for (const id of dockOrder) {
    const app = apps[id];
    entries.push({
      id: `app.open.${id}`,
      title: `Open ${app.title}`,
      summary: app.summary,
      risk: "write",
      group: "Apps",
      keywords: [id, "app", "window", "open"],
      run: () => windows.open(id).then(() => undefined),
    });
  }

  return entries;
}
