import { tmpdir } from "node:os";
import { join, resolve } from "node:path";
import { fileURLToPath } from "node:url";

/**
 * Where the harness finds things, from the environment or the repository's
 * own layout. One module, so the config, the global setup and the fixtures
 * cannot disagree about which binary or which port they mean.
 */

/** `web/`. */
export const WEB = resolve(fileURLToPath(new URL("../..", import.meta.url)));

/** The repository root. */
export const ROOT = resolve(WEB, "..");

/** Cargo's target directory: `$CARGO_TARGET_DIR`, else `<root>/target`. */
export const TARGET = process.env.CARGO_TARGET_DIR
  ? resolve(process.env.CARGO_TARGET_DIR)
  : join(ROOT, "target");

/** The build the harness runs: debug, or release with `DJ_E2E_RELEASE=1` (for measuring). */
export const RELEASE = process.env.DJ_E2E_RELEASE === "1";
export const BIN = join(TARGET, RELEASE ? "release" : "debug");

/**
 * The apps installed into every scratch node. The shell smoke tests use both.
 * `crate` is the directory under `crates/` (`deckjack-<crate>`); `name` is
 * the app's name in its manifest, which its package and binary follow
 * (`deckjack-<name>`). They differ because `files` and `editor` are the
 * core's own namespaces, which no app may take as its name.
 */
export const APPS = [
  { crate: "files", name: "filer" },
  { crate: "editor", name: "scribe" },
  // On the UI protocol: no page, its window drawn by the shell.
  { crate: "tasks", name: "tasks" },
] as const;

/**
 * The first port a core listens on. Worker N uses BASE + N, so one worker
 * (the default) uses exactly this port. Never 7878, which is a real node's.
 */
export const BASE_PORT = Number(process.env.DJ_E2E_PORT ?? 8790);
if (!Number.isInteger(BASE_PORT) || BASE_PORT <= 0 || BASE_PORT === 7878) {
  throw new Error(`DJ_E2E_PORT must be a port other than 7878, not ${process.env.DJ_E2E_PORT}`);
}

/**
 * Where scratch nodes live. Short by default, because an app's Unix socket
 * goes under it and a socket path is limited to 108 bytes.
 */
export const LIVE_ROOT = resolve(process.env.DJ_E2E_LIVE ?? join(tmpdir(), "dj-e2e"));

/** Keep each test's node directory instead of deleting it (for debugging). */
export const KEEP = process.env.DJ_E2E_KEEP === "1";

/**
 * Libraries that let Playwright's Ubuntu-built WebKit run on a host without
 * them (see `webkit/fetch-compat-libs.sh`). Used only if the directory exists.
 */
export const WEBKIT_COMPAT = resolve(
  process.env.DJ_WEBKIT_COMPAT_LIBS ??
    join(process.env.XDG_CACHE_HOME ?? join(process.env.HOME ?? tmpdir(), ".cache"), "deckjack-e2e", "webkit-compat"),
);
