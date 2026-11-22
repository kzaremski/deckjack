import { computed, ref, type ComputedRef, type Ref } from "vue";

/**
 * Quick Look: look at a file without launching the thing that owns it.
 *
 * # Why this is in the shell and not an app
 *
 * The browser already decodes images, audio, video and PDF. An app binary
 * here would add a process, a socket and a theme boundary to arrive at the
 * same `<img>`. It is also cross-cutting — reachable from Files, from the
 * palette and from any app — which an app-owned window is not.
 *
 * What the shell owns is therefore only *judgement*: which files it can
 * honestly show, which it must admit it cannot, and which it must refuse to
 * execute. The bytes come from the core's `/content` route, which is where
 * the session gate, the range requests and the sandbox policy live.
 *
 * # The one rule that is not a convenience
 *
 * A file in the working directory is attacker-authored as far as the shell is
 * concerned: it could have arrived by download, email, sync or an agent.
 * Previewing an HTML file or an SVG on the shell's own origin would run its
 * script with the operator's session. So anything that can carry code is
 * marked [`Viewer::sandboxed`] and rendered in a sandboxed frame, and the
 * content route sends `Content-Security-Policy: sandbox` with it. Both halves
 * are needed: the header alone does not stop the frame from being same-origin,
 * and the frame alone does not stop a browser that sniffs.
 */

/** The families of thing a preview can be. */
export type ViewerKind =
  | "image"
  | "video"
  | "audio"
  | "pdf"
  | "text"
  | "markdown"
  | "archive"
  | "web"
  | "none";

export interface Viewer {
  kind: ViewerKind;
  /** Whether this must render inside a sandboxed frame. */
  sandboxed: boolean;
  /** Why there is no preview, when `kind` is `none`. Always set if it is. */
  why?: string;
}

const IMAGE = new Set(["jpg", "jpeg", "png", "gif", "webp", "avif", "bmp", "ico"]);
const VIDEO = new Set(["mp4", "m4v", "webm", "ogv", "mov"]);
const AUDIO = new Set(["mp3", "wav", "flac", "ogg", "oga", "opus", "m4a", "aac"]);
/** Anything that can carry script, and so is never rendered unsandboxed. */
const EXECUTABLE_MARKUP = new Set(["html", "htm", "svg", "xml", "xhtml"]);
/**
 * Camera RAW. Named explicitly rather than falling through to "unknown",
 * because the operator's photographs are the files most likely to be opened
 * here and the honest answer is specific: the node has to render these.
 */
const RAW = new Set(["nef", "cr2", "cr3", "arw", "dng", "raf", "orf", "rw2", "pef", "srw"]);
/** Containers no browser decodes, worth naming so the refusal can say why. */
const UNDECODABLE_MEDIA = new Set(["mkv", "avi", "wmv", "flv", "mpg", "mpeg", "ts", "m2ts"]);
/** Archives the node's `files.archive.list` reads. `gz` alone is a gzip of
 * one file, which it lists as that file. */
const ARCHIVE = new Set(["zip", "tar", "tgz", "gz", "jar", "apk", "whl", "epub"]);
/** Archive formats it does not, named so the refusal can say which. */
const UNLISTABLE: Record<string, string> = {
  "7z": "7-Zip",
  rar: "RAR",
  xz: "xz",
  txz: "xz",
  zst: "zstd",
  tzst: "zstd",
  bz2: "bzip2",
  tbz2: "bzip2",
  lz: "lzip",
  lzma: "LZMA",
  cab: "CAB",
  iso: "ISO image",
  dmg: "disk image",
};
const MARKDOWN = new Set(["md", "markdown", "mdown", "mkd", "mkdn"]);
/** Office formats, which need a converter on the node (the office suite). */
const OFFICE = new Set([
  "docx",
  "doc",
  "odt",
  "xlsx",
  "xls",
  "ods",
  "pptx",
  "ppt",
  "odp",
  "rtf",
  "pages",
  "numbers",
  "key",
]);

/**
 * Extensionless files that are text by convention. A `Makefile` opens in an
 * editor everywhere else; refusing it here would be a surprise.
 */
const BARE_TEXT = new Set([
  "makefile",
  "dockerfile",
  "license",
  "licence",
  "readme",
  "changelog",
  "notice",
  "authors",
  "codeowners",
  "gitignore",
  "gitattributes",
  "editorconfig",
]);

function extensionOf(name: string): string {
  const base = name.split("/").pop() ?? name;
  const dot = base.lastIndexOf(".");
  return dot > 0 ? base.slice(dot + 1).toLowerCase() : "";
}

/**
 * Which viewer a file gets.
 *
 * `mime` is the server's answer and wins when the two disagree, because it is
 * what set the `Content-Type` the browser will actually obey. The extension is
 * only how a viewer is chosen before any bytes have arrived.
 */
export function viewerFor(name: string, mime?: string): Viewer {
  if (mime) {
    const fromServer = fromMime(mime);
    if (fromServer) return fromServer;
  }

  const ext = extensionOf(name);
  const bare = (name.split("/").pop() ?? name).toLowerCase();

  if (IMAGE.has(ext)) return { kind: "image", sandboxed: false };
  if (VIDEO.has(ext)) return { kind: "video", sandboxed: false };
  if (AUDIO.has(ext)) return { kind: "audio", sandboxed: false };
  if (ext === "pdf") return { kind: "pdf", sandboxed: false };
  if (EXECUTABLE_MARKUP.has(ext)) return { kind: "web", sandboxed: true };
  if (MARKDOWN.has(ext)) return { kind: "markdown", sandboxed: false };
  if (ARCHIVE.has(ext)) return { kind: "archive", sandboxed: false };

  if (RAW.has(ext)) {
    return {
      kind: "none",
      sandboxed: false,
      why: `No browser decodes ${ext.toUpperCase()} camera raw. The node has to develop a preview first.`,
    };
  }
  if (UNDECODABLE_MEDIA.has(ext)) {
    return {
      kind: "none",
      sandboxed: false,
      why: `A browser cannot play .${ext}. Convert it, or open it in the video app.`,
    };
  }
  const unlistable = UNLISTABLE[ext];
  if (unlistable) {
    return {
      kind: "none",
      sandboxed: false,
      why: `This is a ${unlistable} file. Quick Look lists zip and tar archives; open this one in a terminal to see inside.`,
    };
  }
  if (OFFICE.has(ext)) {
    return {
      kind: "none",
      sandboxed: false,
      why: `Quick Look does not draw .${ext} documents. Open it in Office, where this node has it.`,
    };
  }

  if (ext === "" && BARE_TEXT.has(bare.replace(/^\./, ""))) {
    return { kind: "text", sandboxed: false };
  }
  if (ext !== "") {
    // Everything left is treated as text. Wrong only for an unknown binary,
    // and the viewer itself refuses those once it has the bytes -- which is
    // the right place, because that is where the bytes are.
    return { kind: "text", sandboxed: false };
  }

  return {
    kind: "none",
    sandboxed: false,
    why: "There is no preview for this kind of file yet.",
  };
}

function fromMime(mime: string): Viewer | null {
  const type = mime.split(";")[0]!.trim().toLowerCase();
  if (type === "image/svg+xml") return { kind: "web", sandboxed: true };
  if (type === "text/html" || type === "application/xhtml+xml" || type.endsWith("/xml")) {
    return { kind: "web", sandboxed: true };
  }
  if (type === "application/pdf") return { kind: "pdf", sandboxed: false };
  if (type === "text/markdown" || type === "text/x-markdown") {
    return { kind: "markdown", sandboxed: false };
  }
  if (
    type === "application/zip" ||
    type === "application/x-tar" ||
    type === "application/gzip" ||
    type === "application/x-gzip"
  ) {
    return { kind: "archive", sandboxed: false };
  }
  if (type.startsWith("image/")) return { kind: "image", sandboxed: false };
  if (type.startsWith("video/")) return { kind: "video", sandboxed: false };
  if (type.startsWith("audio/")) return { kind: "audio", sandboxed: false };
  if (type.startsWith("text/")) return { kind: "text", sandboxed: false };
  return null;
}

/**
 * The address of a file's bytes on the node.
 *
 * `URLSearchParams` rather than string interpolation, and that is not style: a
 * filename containing `&`, `?` or `#` is ordinary, and interpolated it would
 * truncate the path so the node is asked for a different file than the one
 * the operator clicked.
 */
export function contentUrl(path: string, options: { download?: boolean } = {}): string {
  const query = new URLSearchParams({ path });
  if (options.download) query.set("download", "1");
  return `/content?${query.toString()}`;
}

/**
 * The working-directory path a relative reference in `from` points at, or
 * null when it is not a file reference (it has a scheme or a host) or climbs
 * above the root. Used for a Markdown file's own images.
 *
 * Climbing out is refused here for a clearer answer, not for safety: the
 * content route resolves every path through the same boundary as every other
 * reader, and would refuse it anyway.
 */
export function resolveRelative(from: string, reference: string): string | null {
  const ref = reference.split(/[?#]/)[0] ?? "";
  if (ref === "" || ref.startsWith("//") || /^[a-z][a-z0-9+.-]*:/i.test(ref)) return null;
  let decoded: string;
  try {
    decoded = decodeURIComponent(ref);
  } catch {
    decoded = ref;
  }
  const base = decoded.startsWith("/") ? [] : from.split("/").slice(0, -1);
  const parts = base.filter((p) => p !== "");
  for (const segment of decoded.split("/")) {
    if (segment === "" || segment === ".") continue;
    if (segment === "..") {
      if (parts.length === 0) return null;
      parts.pop();
    } else parts.push(segment);
  }
  return parts.length ? `/${parts.join("/")}` : null;
}

/** "1.4 MB", in the decimal units a file manager shows. */
export function formatBytes(n: number): string {
  if (!Number.isFinite(n) || n < 0) return "";
  if (n < 1000) return `${n} ${n === 1 ? "byte" : "bytes"}`;
  const units = ["KB", "MB", "GB", "TB"];
  let v = n;
  let u = -1;
  do {
    v /= 1000;
    u += 1;
  } while (v >= 1000 && u < units.length - 1);
  return `${v < 10 ? v.toFixed(1) : Math.round(v)} ${units[u]}`;
}

/** "1:02:03" or "4:05", for a duration in seconds. */
export function formatDuration(seconds: number): string {
  if (!Number.isFinite(seconds) || seconds < 0) return "";
  const s = Math.round(seconds);
  const h = Math.floor(s / 3600);
  const m = Math.floor((s % 3600) / 60);
  const ss = String(s % 60).padStart(2, "0");
  return h > 0 ? `${h}:${String(m).padStart(2, "0")}:${ss}` : `${m}:${ss}`;
}

export interface QuickLookEnv {
  /** Whether this device has a network. The bytes are on the node. */
  online(): boolean;
  /**
   * The shell's app table, to know whether an app that can open a file fully
   * exists on this node. Required, like every field here: a hand-off wired to
   * nothing would be a button that does nothing.
   */
  apps: { isKnown(id: string): boolean; contract(id: string): { title: string } };
  /**
   * Open `app` on `path`. In the shell this is {@link openAppVia} over the
   * window store: Quick Look lives in the shell, so it calls the store
   * directly rather than posting `shell.app.open` to itself.
   */
  openApp(app: string, path: string): void | Promise<unknown>;
}

/**
 * The part of the window store "open fully" uses. `openApp` is the window
 * store's half of the open-app contract -- focus or open the app's window and
 * hand it the path as `?path=` -- and is optional because it arrives with
 * the frame bridge's lane: until then only `open`, which cannot carry a path,
 * exists.
 */
export interface AppOpener {
  open(app: string): Promise<unknown>;
  openApp?(app: string, options: { path?: string }): Promise<unknown>;
}

/**
 * Quick Look's `openApp`, wired to a window store: the open-app entry with
 * the path when the store has one, else a plain open of the app (the file is
 * then not carried, which is what the details of the lane record).
 *
 * Looked up at call time, and shared by `context.ts` and the test harness,
 * so the production wiring is the one the shell tests exercise and starts
 * carrying the path the moment the store gains the entry, with no edit here.
 */
export function openAppVia(windows: AppOpener): QuickLookEnv["openApp"] {
  return (app, path) => (windows.openApp ? windows.openApp(app, { path }) : windows.open(app));
}

/** The Editor's name in the app table: `scribe`, because `editor` is the
 * core's own command namespace and no app may take one (APP-UI-ISOLATION §4.6). */
export const EDITOR_APP = "scribe";

/**
 * Which app opens a file fully, by viewer.
 *
 * Text-like files go to the editor, whose contract is "open any file by
 * path"; office documents go to Office (below, by extension). A photograph,
 * a video or an archive has no owning app in the suite yet, and guessing one
 * would be a button that opens the wrong thing.
 */
const HANDOFF: Partial<Record<ViewerKind, string>> = {
  text: EDITOR_APP,
  markdown: EDITOR_APP,
  web: EDITOR_APP,
};

/** The Office app's name in the app table (Lane 6a). */
export const OFFICE_APP = "office";

/** The app a file goes to by its extension, when its viewer kind has none:
 * office documents, which Quick Look cannot draw and Office can. */
function handoffFor(path: string): string | undefined {
  const kind = viewerFor(path).kind;
  return HANDOFF[kind] ?? (OFFICE.has(extensionOf(path)) ? OFFICE_APP : undefined);
}

export interface QuickLookStore {
  visible: Ref<boolean>;
  current: Ref<string | null>;
  /** Why nothing can be fetched right now, or null when it can. */
  unavailable: ComputedRef<string | null>;
  /** Where in the folder the operator is, for "3 of 40". */
  position: ComputedRef<{ index: number; total: number } | null>;
  /**
   * The app "open fully" hands the current file to, or null when there is
   * none: no such app installed, or no app that owns this kind of file.
   */
  handoff: ComputedRef<{ app: string; title: string } | null>;
  open(path: string, siblings?: readonly string[]): void;
  close(): void;
  /** Hand the current file to its app and close. False when there is none. */
  openFully(): boolean;
  next(): void;
  previous(): void;
}

export function createQuickLook(env: QuickLookEnv): QuickLookStore {
  const visible = ref(false);
  const current = ref<string | null>(null);
  const folder = ref<readonly string[]>([]);

  const unavailable = computed(() =>
    env.online()
      ? null
      : "This device is offline, and the file is on the node — there is nothing to show until it is back.",
  );

  const position = computed(() => {
    const at = folder.value.indexOf(current.value ?? "");
    if (at < 0 || folder.value.length < 2) return null;
    return { index: at + 1, total: folder.value.length };
  });

  /**
   * Clamped, not wrapped. Wrapping makes "next" on the last file look like
   * nothing happened, and hides from the operator that they have reached the
   * end of the folder.
   */
  function step(by: number) {
    const at = folder.value.indexOf(current.value ?? "");
    if (at < 0) return;
    const to = at + by;
    if (to < 0 || to >= folder.value.length) return;
    current.value = folder.value[to]!;
  }

  const handoff = computed(() => {
    if (!current.value) return null;
    const app = handoffFor(current.value);
    if (!app || !env.apps.isKnown(app)) return null;
    return { app, title: env.apps.contract(app).title };
  });

  function close() {
    visible.value = false;
    // Cleared, so reopening does not flash the previous file inside the new
    // one's frame while the new bytes are still in flight.
    current.value = null;
    folder.value = [];
  }

  return {
    visible,
    current,
    unavailable,
    position,
    handoff,
    open(path, siblings) {
      folder.value = siblings ? [...siblings] : [path];
      current.value = path;
      visible.value = true;
    },
    close,
    openFully() {
      const target = handoff.value;
      const path = current.value;
      if (!target || !path) return false;
      close();
      void env.openApp(target.app, path);
      return true;
    },
    next: () => step(1),
    previous: () => step(-1),
  };
}
