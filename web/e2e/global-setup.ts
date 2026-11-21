import { execFileSync } from "node:child_process";

import { APPS, ROOT, WEB } from "./support/paths.ts";

/**
 * Build what the tests run, once, before any of them.
 *
 * The shell is baked into the core binary at compile time, so the order is
 * fixed: the web bundle first, then cargo. Both are incremental, so a run
 * with nothing changed costs a few seconds. `DJ_E2E_SKIP_BUILD=1` skips this
 * when the caller has just built (CI, or a loop over one test).
 */
export default function globalSetup(): void {
  if (process.env.DJ_E2E_SKIP_BUILD === "1") return;
  const run = (cmd: string, args: string[], cwd: string) =>
    execFileSync(cmd, args, { cwd, stdio: "inherit", env: process.env });
  run("npm", ["run", "build"], WEB);
  const packages = ["deckjack-core", ...APPS.map((a) => `deckjack-${a.name}`)].flatMap((p) => ["-p", p]);
  run("cargo", ["build", ...packages, "--bins"], ROOT);
}
