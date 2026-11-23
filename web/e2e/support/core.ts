import { type ChildProcess, execFileSync, spawn } from "node:child_process";
import {
  chmodSync,
  copyFileSync,
  existsSync,
  mkdirSync,
  mkdtempSync,
  readFileSync,
  readdirSync,
  readlinkSync,
  rmSync,
  writeFileSync,
} from "node:fs";
import { connect } from "node:net";
import { join } from "node:path";

import { APPS, BIN, KEEP, LIVE_ROOT, ROOT } from "./paths.ts";
import { seed, type Seeded } from "./seed.ts";

/**
 * A real `deckjack --debug` node, started for one test and stopped after it.
 *
 * One node per test, not per run: the window layout lives on the server, so a
 * node shared between tests would hand each one the windows the last left
 * open, and a failure would depend on test order. A debug node starts in well
 * under a second, so isolation costs little.
 *
 * Each node gets its own install directory (`h/`, the apps and the debug
 * session cookie) and its own working directory (`w/`, the files the shell
 * shows). They are kept apart on purpose: with one directory, Files would
 * list the install's `apps/` and the cookie file next to the test's files.
 */
export interface Core {
  /** `http://127.0.0.1:<port>`: the origin the browser signs in to. */
  origin: string;
  port: number;
  /** Install directory (`DECKJACK_HOME`). */
  home: string;
  /** Working directory (`--dir`), seeded with the fixture files. */
  work: string;
  /** The debug session, ready for `context.addCookies`. */
  cookie: { name: string; value: string };
  /** What `seed` put in the working directory. */
  files: Seeded;
  /**
   * Whether a native app is declared for streaming (`native/zenity.toml`
   * beside the compositor): false on a host without zenity or without the
   * compositor build, where the streaming test skips and says so.
   */
  native: boolean;
  /** Everything the node has logged so far. */
  log(): string;
  /** Stop the node and every process it started, then remove its directory. */
  stop(): Promise<void>;
}

const STARTUP_MS = 30_000;

export async function startCore(port: number): Promise<Core> {
  const binary = join(BIN, "deckjack");
  if (!existsSync(binary)) {
    throw new Error(
      `${binary} does not exist. The global setup builds it; with DJ_E2E_SKIP_BUILD=1, ` +
        "run `npm run build` and `cargo build --workspace --bins` first.",
    );
  }
  if (await answers(port)) {
    throw new Error(
      `something is already listening on 127.0.0.1:${port}. The harness never stops a ` +
        "process it did not start; free the port or set DJ_E2E_PORT.",
    );
  }

  mkdirSync(LIVE_ROOT, { recursive: true });
  const dir = mkdtempSync(join(LIVE_ROOT, `${port}-`));
  const home = join(dir, "h");
  const work = join(dir, "w");
  mkdirSync(home);
  mkdirSync(work);

  try {
    // Through the core's own installer, as `cargo xtask dev` does: it
    // validates the manifest and lays the app out the way discovery expects.
    for (const app of APPS) {
      execFileSync(
        binary,
        ["install", join(ROOT, "crates", `deckjack-${app.crate}`), "--binary", join(BIN, `deckjack-${app.name}`)],
        { env: { ...process.env, DECKJACK_HOME: home }, stdio: "pipe" },
      );
    }
  } catch (error) {
    rmSync(dir, { recursive: true, force: true });
    throw error;
  }
  const native = installNative(home);
  const files = seed(work);

  let output = "";
  const child = spawn(binary, ["--debug", "--listen", `127.0.0.1:${port}`, "--dir", work], {
    cwd: work,
    env: {
      ...process.env,
      DECKJACK_HOME: home,
      // The terminal runs $SHELL. A plain POSIX shell keeps the echo test
      // independent of whoever runs it and their prompt.
      SHELL: "/bin/sh",
      NO_COLOR: "1",
    },
    stdio: ["ignore", "pipe", "pipe"],
  });
  child.stdout?.on("data", (b: Buffer) => (output += b.toString()));
  child.stderr?.on("data", (b: Buffer) => (output += b.toString()));

  const stop = () => shutdown(child, dir, () => output);
  try {
    const cookie = await ready(child, port, join(home, "debug-session.cookie"), () => output);
    return {
      origin: `http://127.0.0.1:${port}`,
      port,
      home,
      work,
      cookie,
      files,
      native,
      log: () => output,
      stop,
    };
  } catch (error) {
    await stop();
    throw error;
  }
}

/**
 * Declare one native app for streaming (docs/lanes/8-stream.md): zenity's
 * entry dialog, a GTK 4 app small enough to start in well under a second.
 * The compositor is copied, not linked, because Landlock checks a link's
 * target and the build directory is granted to nothing.
 */
function installNative(home: string): boolean {
  const compositor = join(BIN, "deckjack-stream");
  if (!existsSync(compositor) || !existsSync("/usr/bin/zenity")) return false;
  const dir = join(home, "native");
  mkdirSync(dir, { recursive: true });
  copyFileSync(compositor, join(dir, "deckjack-stream"));
  chmodSync(join(dir, "deckjack-stream"), 0o755);
  writeFileSync(
    join(dir, "zenity.toml"),
    [
      "[app]",
      'name = "zenity"',
      'version = "4"',
      "protocol = 1",
      'title = "Zenity"',
      'tag = "Ze"',
      "",
      "[native]",
      'exec = ["/usr/bin/zenity", "--entry", "--text=Name"]',
      "",
    ].join("\n"),
  );
  // A second one, whose answer is marked, so a test can tell which of two
  // streamed windows the keys went to.
  writeFileSync(
    join(dir, "zenity2.toml"),
    [
      "[app]",
      'name = "zenity2"',
      'version = "4"',
      "protocol = 1",
      'title = "Zenity Two"',
      'tag = "Z2"',
      "",
      "[native]",
      'exec = ["/usr/bin/sh", "-c", "/usr/bin/zenity --entry --title=Second --text=Name | /usr/bin/sed s/^/second:/"]',
      "",
    ].join("\n"),
  );
  return true;
}

/** Wait for the cookie file and a healthy `/healthz`, or for the node to die. */
async function ready(
  child: ChildProcess,
  port: number,
  cookieFile: string,
  log: () => string,
): Promise<{ name: string; value: string }> {
  const deadline = Date.now() + STARTUP_MS;
  while (Date.now() < deadline) {
    if (child.exitCode !== null || child.signalCode !== null) {
      throw new Error(`the core exited during startup (${child.exitCode ?? child.signalCode}):\n${log()}`);
    }
    if (existsSync(cookieFile)) {
      try {
        const res = await fetch(`http://127.0.0.1:${port}/healthz`);
        if (res.ok) return parseCookie(readFileSync(cookieFile, "utf8"));
      } catch {
        // Not listening yet.
      }
    }
    await sleep(100);
  }
  throw new Error(`the core did not come up on 127.0.0.1:${port} within ${STARTUP_MS} ms:\n${log()}`);
}

/** `name=value\n`, as `debugmode::write_cookie` writes it. */
export function parseCookie(text: string): { name: string; value: string } {
  const line = text.trim();
  const eq = line.indexOf("=");
  if (eq <= 0 || eq === line.length - 1) throw new Error(`not a cookie: ${JSON.stringify(line)}`);
  return { name: line.slice(0, eq), value: line.slice(eq + 1) };
}

/**
 * Stop the node and whatever it started.
 *
 * The descendants (app processes, terminal shells) are listed before the
 * node is signalled, because once it exits they are reparented and no longer
 * findable as its children. Only those, by PID, and only if their executable
 * or working directory is still the one this test made: a PID reused by an
 * unrelated process in the meantime is left alone.
 */
async function shutdown(child: ChildProcess, dir: string, log: () => string): Promise<void> {
  const pid = child.pid;
  const descendants = pid ? descendantsOf(pid) : [];
  if (pid && child.exitCode === null && child.signalCode === null) {
    child.kill("SIGTERM");
    if (!(await exited(child, 5_000))) {
      child.kill("SIGKILL");
      await exited(child, 5_000);
    }
  }
  for (const p of descendants) {
    if (belongsTo(p, dir)) {
      try {
        process.kill(p, "SIGKILL");
      } catch {
        // Already gone.
      }
    }
  }
  if (KEEP) writeFileSync(join(dir, "core.log"), log());
  else rmSync(dir, { recursive: true, force: true });
}

function exited(child: ChildProcess, ms: number): Promise<boolean> {
  if (child.exitCode !== null || child.signalCode !== null) return Promise.resolve(true);
  return new Promise((resolve) => {
    const timer = setTimeout(() => resolve(false), ms);
    child.once("exit", () => {
      clearTimeout(timer);
      resolve(true);
    });
  });
}

/** Every process below `root`, from /proc. Linux only, as is the core. */
function descendantsOf(root: number): number[] {
  const parents = new Map<number, number>();
  for (const entry of readdirSync("/proc")) {
    if (!/^\d+$/.test(entry)) continue;
    try {
      const stat = readFileSync(`/proc/${entry}/stat`, "utf8");
      // The command name is in parentheses and may contain spaces; the
      // parent PID is the second field after it.
      const after = stat.slice(stat.lastIndexOf(")") + 2).split(" ");
      parents.set(Number(entry), Number(after[1]));
    } catch {
      // Exited while we looked.
    }
  }
  const out: number[] = [];
  const queue = [root];
  while (queue.length) {
    const p = queue.shift()!;
    for (const [child, parent] of parents) {
      if (parent === p) {
        out.push(child);
        queue.push(child);
      }
    }
  }
  return out;
}

function belongsTo(pid: number, dir: string): boolean {
  for (const link of ["exe", "cwd"]) {
    try {
      if (readlinkSync(`/proc/${pid}/${link}`).startsWith(dir)) return true;
    } catch {
      // Gone, or not ours to read.
    }
  }
  return false;
}

/** Whether anything accepts a connection on the port. */
function answers(port: number): Promise<boolean> {
  return new Promise((resolve) => {
    const socket = connect({ host: "127.0.0.1", port });
    socket.once("connect", () => {
      socket.destroy();
      resolve(true);
    });
    socket.once("error", () => resolve(false));
  });
}

function sleep(ms: number) {
  return new Promise((r) => setTimeout(r, ms));
}
