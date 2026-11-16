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
export type ViewerKind = "image" | "video" | "audio" | "pdf" | "text" | "web" | "none";

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
const ARCHIVE = new Set(["zip", "7z", "rar", "tar", "gz", "bz2", "xz", "zst"]);

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
  if (ARCHIVE.has(ext)) {
    return {
      kind: "none",
      sandboxed: false,
      why: "Archives are listed rather than previewed, which Files does not do yet.",
    };
  }

  if (ext === "" && BARE_TEXT.has(bare.replace(/^\./, ""))) {
    return { kind: "text", sandboxed: false };
  }
  if (ext !== "" && !ARCHIVE.has(ext)) {
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

export interface QuickLookEnv {
  /** Whether this device has a network. The bytes are on the node. */
  online(): boolean;
}

export interface QuickLookStore {
  visible: Ref<boolean>;
  current: Ref<string | null>;
  /** Why nothing can be fetched right now, or null when it can. */
  unavailable: ComputedRef<string | null>;
  /** Where in the folder the operator is, for "3 of 40". */
  position: ComputedRef<{ index: number; total: number } | null>;
  open(path: string, siblings?: readonly string[]): void;
  close(): void;
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

  return {
    visible,
    current,
    unavailable,
    position,
    open(path, siblings) {
      folder.value = siblings ? [...siblings] : [path];
      current.value = path;
      visible.value = true;
    },
    close() {
      visible.value = false;
      // Cleared, so reopening does not flash the previous file inside the new
      // one's frame while the new bytes are still in flight.
      current.value = null;
      folder.value = [];
    },
    next: () => step(1),
    previous: () => step(-1),
  };
}
