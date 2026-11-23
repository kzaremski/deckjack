import { computed, ref, type ComputedRef, type Ref } from "vue";

import type { Size } from "../shell/geometry.ts";
import { nativeAppId } from "../shell/stream.ts";

/**
 * The app contract.
 *
 * Every app declares a compact layout, a wide layout and a minimum width, and
 * responds to whatever space it is given. The window manager needs those
 * facts before an app's backend answers, so they live here as data rather
 * than inside each panel.
 *
 * Where they come from is the point of this module. They used to be a closed
 * union compiled into the bundle, which meant an app installed after the
 * shell was built could never reach the dock, the switcher or the palette.
 * Now the table is the built-in entries below merged with what the core's
 * `system.apps` reports from each installed `app.toml`, and the manifest wins
 * wherever it says something.
 */

/** An app's name: the directory, the binary suffix, the URL segment and the
 * command namespace. Any string the core reports, not a fixed list. */
export type AppId = string;

export interface AppContract {
  id: AppId;
  title: string;
  /** Two characters for the window tag and the dock. Short enough to read at
   * 22px, which an icon font would not be at compact density. */
  tag: string;
  summary: string;
  minSize: Size;
  defaultSize: Size;
  /** False when the app refuses to render in that layout at all. */
  compact: boolean;
  wide: boolean;
  /** Shows the Inbox count on the dock, per "pending approvals surface as a
   * dock badge and a notification". */
  badge?: "inbox";
  /** Sorted after every other app in the dock. */
  last?: boolean;
  /**
   * A native Linux app streamed from the node (docs/lanes/8-stream.md),
   * not a web UI: its id is `native:<name>`, the dock and its windows say
   * "streamed", and opening it runs `stream.launch` instead of opening an
   * empty window.
   */
  streamed?: string;
}

/** A native app as `stream.apps` reports it. */
export interface NativeApp {
  name: string;
  title: string;
  tag?: string;
  x11: boolean;
}

/** Parse `stream.apps`. Anything malformed is dropped. */
export function parseNative(result: unknown): NativeApp[] {
  const list = (result as { apps?: unknown } | undefined)?.apps;
  if (!Array.isArray(list)) return [];
  return list
    .filter((a): a is Record<string, unknown> => typeof a === "object" && a !== null)
    .filter((a) => typeof a.name === "string" && a.name !== "")
    .map((a) => ({
      name: a.name as string,
      title: typeof a.title === "string" && a.title !== "" ? a.title : (a.name as string),
      tag: typeof a.tag === "string" && a.tag !== "" ? a.tag : undefined,
      x11: a.x11 === true,
    }));
}

function fromNative(app: NativeApp): AppContract {
  return {
    id: nativeAppId(app.name),
    title: app.title,
    tag: app.tag ?? tagFrom(app.title),
    summary: `${app.title}, a native app streamed from this node`,
    minSize: { w: 120, h: 80 },
    defaultSize: { ...DEFAULT_SIZE },
    compact: true,
    wide: true,
    streamed: app.name,
  };
}

/** What a new window gets when nothing says otherwise. */
const DEFAULT_MIN: Size = { w: 320, h: 200 };
const DEFAULT_SIZE: Size = { w: 640, h: 420 };

/**
 * The shell's own entries, in dock order.
 *
 * Two kinds. The terminal and the Assistant's Inbox are the core's, so they
 * exist whatever is installed. The rest are apps the design names; they keep
 * their dock slot so the desktop has the same shape before and after an app
 * binary is installed, and an installed manifest overrides any field here.
 */
export const builtinApps: readonly AppContract[] = [
  {
    id: "filer",
    title: "Files",
    tag: "Fi",
    summary: "The working directory",
    minSize: { w: 360, h: 220 },
    defaultSize: { w: 680, h: 440 },
    compact: true,
    wide: true,
  },
  {
    id: "terminal",
    title: "Terminal",
    tag: "Te",
    // A terminal narrower than 80 columns is a different tool, so the
    // minimum is a real constraint rather than a guess.
    summary: "A real shell on the host, over a PTY",
    minSize: { w: 420, h: 200 },
    defaultSize: { w: 640, h: 400 },
    compact: true,
    wide: true,
  },
  {
    id: "notes",
    title: "Notes",
    tag: "No",
    summary: "Documents in the working directory",
    minSize: { w: 340, h: 220 },
    defaultSize: { w: 620, h: 480 },
    compact: true,
    wide: true,
  },
  {
    id: "tasks",
    title: "Tasks",
    tag: "Ta",
    summary: "Task lists and reminders",
    minSize: { w: 320, h: 220 },
    defaultSize: { w: 480, h: 520 },
    compact: true,
    wide: true,
  },
  {
    id: "mail",
    title: "Mail",
    tag: "Ma",
    summary: "The local mirror of the mailbox",
    minSize: { w: 360, h: 240 },
    defaultSize: { w: 760, h: 520 },
    compact: true,
    wide: true,
  },
  {
    id: "calendar",
    title: "Calendar",
    tag: "Ca",
    summary: "Events and alarms, as iCalendar",
    minSize: { w: 360, h: 260 },
    defaultSize: { w: 820, h: 540 },
    compact: true,
    wide: true,
  },
  {
    id: "assistant",
    title: "Assistant",
    tag: "As",
    summary: "Agent sessions and the Inbox of pending approvals",
    minSize: { w: 340, h: 320 },
    defaultSize: { w: 420, h: 540 },
    compact: true,
    wide: true,
    badge: "inbox",
    // The Assistant is one app among many, so it sits at the end rather
    // than first, and an installed app joins the dock before it.
    last: true,
  },
];

/**
 * An app the core has discovered on this node, as `system.apps` reports it.
 *
 * Optional fields are the manifest's own words: absent means the manifest
 * said nothing, and the built-in entry or the shell's default applies.
 */
export interface InstalledApp {
  name: string;
  title: string;
  version: string;
  lifecycle: "service" | "on-demand";
  status: "live" | "reconnecting" | "unavailable";
  minWidth?: number;
  tag?: string;
  summary?: string;
  defaultWidth?: number;
  defaultHeight?: number;
  compact: boolean;
  wide: boolean;
  /**
   * Where it stands with the operator's approval. `required` (never
   * approved) and `stale` (approved for a different manifest, or before the
   * fingerprint became SHA-256) mean it will not run until approved; the
   * palette offers "Approve <title>" for those. `parseInstalled` always
   * sets it; absent, as in an entry built by hand, means unknown.
   */
  approval?: AppApproval;
  /** What it asks for, in the core's words, for the approval entry. */
  asks?: string;
  /**
   * How its window is drawn: absent, the app's own page in a frame; 1, a
   * tree the app describes over the UI protocol, drawn by the shell.
   */
  tier?: number;
}

export type AppApproval = "not-needed" | "approved" | "required" | "stale" | "unknown";

const APPROVALS: readonly AppApproval[] = ["not-needed", "approved", "required", "stale"];

/** Whether the operator has to approve the app before it can run. */
export function awaitsApproval(app: InstalledApp): boolean {
  return app.approval === "required" || app.approval === "stale";
}

/** Parse `system.apps`. Anything malformed is dropped, not guessed at. */
export function parseInstalled(result: unknown): InstalledApp[] {
  const list = (result as { apps?: unknown } | undefined)?.apps;
  if (!Array.isArray(list)) return [];
  const num = (v: unknown) => (typeof v === "number" && v > 0 ? v : undefined);
  const str = (v: unknown) => (typeof v === "string" && v !== "" ? v : undefined);
  return list
    .filter((a): a is Record<string, unknown> => typeof a === "object" && a !== null)
    .map<InstalledApp>((a) => ({
      name: String(a.name ?? ""),
      title: str(a.title) ?? String(a.name ?? ""),
      version: String(a.version ?? ""),
      lifecycle: a.lifecycle === "service" ? "service" : "on-demand",
      status:
        a.status === "live" || a.status === "reconnecting" ? a.status : "unavailable",
      minWidth: num(a.min_width),
      tag: str(a.tag),
      summary: str(a.summary),
      defaultWidth: num(a.default_width),
      defaultHeight: num(a.default_height),
      // Absent is "both", as the core's own default says. Only an explicit
      // false takes a layout away.
      compact: a.compact !== false,
      wide: a.wide !== false,
      // Anything this shell does not know is "unknown", which offers no
      // approval entry: guessing "required" would put a button in front of
      // the operator for a state the core never reported.
      approval: APPROVALS.find((x) => x === a.approval) ?? "unknown",
      asks: str(a.asks),
      tier: num(a.tier),
    }))
    .filter((a) => a.name !== "");
}

/** Two characters from a title: "Photos" is "Ph", "x" is "X". */
export function tagFrom(title: string): string {
  const letters = [...title.trim()].filter((c) => /[\p{L}\p{N}]/u.test(c));
  if (letters.length === 0) return "?";
  return letters[0]!.toUpperCase() + (letters[1]?.toLowerCase() ?? "");
}

/** The contract for one installed app, over its built-in entry if any. */
function fromManifest(app: InstalledApp, base: AppContract | undefined): AppContract {
  const minW = app.minWidth ?? base?.minSize.w ?? DEFAULT_MIN.w;
  const minH = base?.minSize.h ?? DEFAULT_MIN.h;
  const title = app.title || base?.title || app.name;
  return {
    id: app.name,
    title,
    tag: app.tag ?? base?.tag ?? tagFrom(title),
    summary: app.summary ?? base?.summary ?? `${title} ${app.version}`.trim(),
    minSize: { w: minW, h: minH },
    defaultSize: {
      // Never open narrower than the app says it can work at.
      w: Math.max(app.defaultWidth ?? base?.defaultSize.w ?? DEFAULT_SIZE.w, minW),
      h: Math.max(app.defaultHeight ?? base?.defaultSize.h ?? DEFAULT_SIZE.h, minH),
    },
    compact: app.compact,
    wide: app.wide,
    badge: base?.badge,
    last: base?.last,
  };
}

export interface AppTable {
  /** What `system.apps` last reported. */
  installed: Ref<InstalledApp[]>;
  /** What `stream.apps` last reported: the native apps this node streams. */
  native: Ref<NativeApp[]>;
  /** Every app the dock, the switcher and the palette offer, in dock order. */
  dock: ComputedRef<AppContract[]>;
  /** The contract for any app id, including one nothing knows: a window
   * whose app was uninstalled while it was open still has to render. */
  contract(id: AppId): AppContract;
  /** Built-in or installed: something the operator can open. */
  isKnown(id: string): boolean;
  /** Installed on this node, so its window shows the app's own UI. */
  isInstalled(id: string): boolean;
  /** Installed, and describes its window over the UI protocol (`tier = 1`). */
  isDescribed(id: string): boolean;
  /**
   * Ask the core what is installed.
   *
   * A refusal leaves the list empty rather than raising a banner:
   * `system.apps` is absent on a core with no app host, and "no installed
   * apps" is then the honest answer.
   */
  load(
    call: (command: string, params?: Record<string, unknown>) => Promise<unknown>,
  ): Promise<void>;
}

export function createAppTable(): AppTable {
  const installed = ref<InstalledApp[]>([]);
  const native = ref<NativeApp[]>([]);
  const builtinById = new Map(builtinApps.map((a) => [a.id, a]));

  const table = computed(() => {
    const byId = new Map<string, AppContract>(builtinById);
    for (const app of installed.value) {
      byId.set(app.name, fromManifest(app, builtinById.get(app.name)));
    }
    // `native:` ids cannot collide with an app's name, which has no colon.
    for (const app of native.value) {
      byId.set(nativeAppId(app.name), fromNative(app));
    }
    return byId;
  });

  const dock = computed(() => {
    const builtinOrder = builtinApps.map((a) => a.id);
    const builtin = builtinOrder.map((id) => table.value.get(id)!);
    // Installed apps the shell has no entry for, alphabetically by title, so
    // two devices list them the same way.
    const extra = [...table.value.values()]
      .filter((a) => !builtinById.has(a.id))
      .sort((a, b) => a.title.localeCompare(b.title));
    const all = [...builtin, ...extra];
    return [...all.filter((a) => !a.last), ...all.filter((a) => a.last)];
  });

  return {
    installed,
    native,
    dock,
    contract(id) {
      return (
        table.value.get(id) ?? {
          id,
          title: id,
          tag: tagFrom(id),
          summary: "An app this node does not have installed",
          minSize: { ...DEFAULT_MIN },
          defaultSize: { ...DEFAULT_SIZE },
          compact: true,
          wide: true,
        }
      );
    },
    isKnown: (id) => table.value.has(id),
    isInstalled: (id) => installed.value.some((a) => a.name === id),
    isDescribed: (id) => installed.value.some((a) => a.name === id && a.tier === 1),
    async load(call) {
      try {
        installed.value = parseInstalled(await call("system.apps"));
      } catch {
        installed.value = [];
      }
      // Absent on a node with no stream registry, and then "none" is the
      // honest answer, as for `system.apps`.
      try {
        native.value = parseNative(await call("stream.apps"));
      } catch {
        native.value = [];
      }
    },
  };
}
