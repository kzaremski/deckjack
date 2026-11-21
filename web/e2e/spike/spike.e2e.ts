import { mkdirSync, readdirSync, readFileSync, statSync, writeFileSync } from "node:fs";
import { join } from "node:path";

import { test as base, expect, type TestInfo } from "@playwright/test";

import { WEB } from "../support/paths.ts";
import { type Hit, type Spike, startSpike } from "./server.ts";

/**
 * The browser-behavior spike behind docs/APP-UI-ISOLATION.md §8 and §7.2
 * item 8. Five questions, each answered empirically in Chromium and WebKit
 * against a throwaway server (`server.ts`), never the core:
 *
 * 1. Does a sandboxed iframe (allow-scripts, no allow-same-origin) send
 *    SameSite=Strict cookies on its subresource and navigation requests to
 *    the parent's origin?
 * 2. Does the parent's CSP frame-src block a child frame that navigates
 *    itself to a disallowed origin (or path)?
 * 3. Are CSP host-source paths honored in script-src, top-level and in the
 *    exact app-frame shape of §4.2?
 * 4. Does `Origin: null` arrive on a WebSocket from an opaque-origin frame,
 *    and does `connect-src 'none'` stop the socket before it is sent?
 * 5. Does the Vue production build contain any inline script, and does it
 *    run under the §4.10 shell CSP?
 *
 * Each test records what it saw (with the engine version) as a JSON
 * attachment and in `test-results/spike-findings/`, then asserts it. The
 * assertions pin the answers found on 2026-11-16, per engine where the
 * engines differ, so an engine update that changes an answer the design
 * leans on fails here first. Every answer is paired with a control that must
 * hold first, so "no cookie" cannot mean "no cookie was ever set".
 */

const test = base.extend<object, { spike: Spike }>({
  spike: [
    async ({}, use) => {
      const spike = await startSpike();
      await use(spike);
      await spike.close();
    },
    { scope: "worker" },
  ],
});

/** What a set of requests carried, reduced to the facts the questions need. */
interface Seen {
  requests: number;
  strict: boolean;
  lax: boolean;
  origin: string | null;
  site: string | null;
  dest: string | null;
}

function seen(hits: Hit[]): Seen {
  const first = hits[0];
  return {
    requests: hits.length,
    strict: hits.some((h) => /(^|;\s*)strict=1/.test(h.cookie ?? "")),
    lax: hits.some((h) => /(^|;\s*)lax=1/.test(h.cookie ?? "")),
    origin: first?.origin ?? null,
    site: first?.secFetchSite ?? null,
    dest: first?.secFetchDest ?? null,
  };
}

async function record(info: TestInfo, question: string, engine: string, findings: unknown) {
  const body = JSON.stringify({ question, project: info.project.name, engine, findings }, null, 2);
  await info.attach(`${question}.json`, { body, contentType: "application/json" });
  const dir = join(WEB, "test-results", "spike-findings");
  mkdirSync(dir, { recursive: true });
  writeFileSync(join(dir, `${info.project.name}-${question}.json`), body);
}

const isWebKit = (name: string) => name === "webkit";

test("Q1: SameSite=Strict cookies from a sandboxed frame to the parent's origin", async ({
  page,
  spike,
  browser,
  browserName,
}, info) => {
  await page.goto(`${spike.A}/set-cookie?next=/q1-parent.html`);
  // Both frames finish by navigating themselves; that is the last request.
  await expect.poll(() => spike.tagged("q1-sbx-nav").length).toBeGreaterThan(0);
  await expect.poll(() => spike.tagged("q1-plain-nav").length).toBeGreaterThan(0);

  const kinds = ["doc", "img", "script", "style", "fetch-nocors", "fetch-cors", "nav"] as const;
  const of = (who: "sbx" | "plain") =>
    Object.fromEntries(kinds.map((k) => [k, seen(spike.tagged(`q1-${who}-${k}`))])) as Record<(typeof kinds)[number], Seen>;
  const findings = {
    parentImg: seen(spike.tagged("q1-parent-img")),
    sandboxed: of("sbx"),
    plain: of("plain"),
  };
  await record(info, "q1", `${browserName} ${browser.version()}`, findings);

  // Controls: the cookies were set, and a same-origin frame sends them.
  expect(findings.parentImg.strict).toBe(true);
  expect(findings.plain.img.strict).toBe(true);
  expect(findings.plain.nav.strict).toBe(true);
  // The frame's own document is requested by the parent, so it carries
  // the cookie whatever the sandbox does to what the frame then loads.
  expect(findings.sandboxed.doc.strict).toBe(true);

  // Every probe from the sandboxed frame reached the server, so "no cookie"
  // below cannot mean "no request".
  for (const k of kinds) expect(findings.sandboxed[k].requests, k).toBeGreaterThan(0);

  // The answer, which differs by engine.
  const engine = isWebKit(browserName) ? EXPECTED.webkit : EXPECTED.chromium;
  const strictFromSandbox = kinds.filter((k) => k !== "doc" && findings.sandboxed[k].strict);
  expect(strictFromSandbox).toEqual(engine.q1StrictFromSandbox);
  // What both engines agree on, and what the §4.9 gate keys on: every
  // request the sandboxed frame makes is labeled cross-site, cookie or not.
  for (const k of kinds.filter((k) => k !== "doc")) {
    expect(findings.sandboxed[k].site, k).toBe("cross-site");
  }
  // And a CORS request from it says `Origin: null`.
  expect(findings.sandboxed["fetch-cors"].origin).toBe("null");
  // The frame's own navigation is an `iframe` destination, not `document`,
  // so §4.9's exception for top-level navigations does not let it through.
  expect(findings.sandboxed.nav.dest).toBe("iframe");
});

test("Q2: the parent's frame-src against a child that navigates itself away", async ({
  page,
  spike,
  browser,
  browserName,
}, info) => {
  spike.headers("/q2-parent.html", { "content-security-policy": `frame-src ${spike.A}/frames/` });
  await page.goto(`${spike.A}/q2-parent.html`);
  // Control: a navigation inside the allowed path lands.
  await expect.poll(() => spike.tagged("q2-ok").length).toBeGreaterThan(0);
  // Every child loaded and asked to leave.
  for (const name of ["q2-cross", "q2-path"]) {
    await expect.poll(() => spike.tagged(`${name}-start`).length).toBeGreaterThan(0);
  }
  // The navigations were started 200 ms after each child loaded; give a
  // refused one ample time to show up if it was not refused.
  await page.waitForTimeout(1_500);
  const violations = await page.evaluate(
    () => (window as unknown as { violations: { directive: string; blocked: string }[] }).violations,
  );
  const findings = {
    crossOriginReachedServer: spike.tagged("q2-cross").length,
    sameOriginOtherPathReachedServer: spike.tagged("q2-path").length,
    allowedPathReachedServer: spike.tagged("q2-ok").length,
    violations,
  };
  await record(info, "q2", `${browserName} ${browser.version()}`, findings);

  const engine = isWebKit(browserName) ? EXPECTED.webkit : EXPECTED.chromium;
  expect(findings.crossOriginReachedServer).toBe(engine.q2CrossReached);
  expect(findings.sameOriginOtherPathReachedServer).toBe(engine.q2PathReached);
  expect(violations.filter((v) => v.directive === "frame-src").length).toBe(engine.q2FrameSrcViolations);
});

test("Q3: host-source paths in script-src, top level", async ({ page, spike, browser, browserName }, info) => {
  spike.headers("/q3-top.html", {
    "content-security-policy": `script-src ${spike.A}/js/allowed/ ${spike.A}/js/exact.js`,
  });
  await page.goto(`${spike.A}/q3-top.html`);
  await expect.poll(() => spike.tagged("q3-allowed").length).toBeGreaterThan(0);
  await page.waitForLoadState("load");
  const ran = await page.evaluate(() => (self as unknown as { ran?: string[] }).ran ?? []);
  const findings = {
    ran,
    requested: ["q3-denied", "q3-allowed", "q3-exact", "q3-exact2"].filter((t) => spike.tagged(t).length > 0),
  };
  await record(info, "q3-top", `${browserName} ${browser.version()}`, findings);

  // Control: the allowed scripts ran, so the page and the CSP both worked.
  expect(ran).toContain("/js/allowed/mark.js");
  expect(ran).toContain("/js/exact.js");
  // The answer: paths are enforced, a directory and an exact file alike.
  expect(ran).not.toContain("/js/denied/mark.js");
  expect(ran).not.toContain("/js/exact2.js");
  // And refused before they are fetched: only the allowed two reached the
  // server.
  expect(findings.requested).toEqual(["q3-allowed", "q3-exact"]);
});

test("Q3: host-source paths in the app frame of §4.2 (opaque origin, module scripts)", async ({
  page,
  spike,
  browser,
  browserName,
}, info) => {
  const A = spike.A;
  const cap = `${A}/apps/demo/~cap/`;
  // The §4.2 response headers, with {O} = A and the cap path above.
  spike.headers("/apps/demo/~cap/index.html", {
    "content-security-policy":
      `sandbox allow-scripts allow-forms; default-src 'none'; script-src ${A}/kit/ ${cap}; ` +
      `style-src ${A}/kit/ ${cap} 'unsafe-inline'; img-src ${A}/kit/ ${cap} data: blob:; ` +
      `connect-src 'none'; worker-src blob:; frame-src 'none'; form-action 'none'; ` +
      `base-uri 'none'; frame-ancestors ${A}`,
    "referrer-policy": "no-referrer",
    "x-content-type-options": "nosniff",
  });
  const cors = { "access-control-allow-origin": "*" };
  for (const path of ["/apps/demo/~cap/main.js", "/apps/demo/~cap/report.js", "/apps/demo/~other/evil.js"]) {
    spike.headers(path, cors);
  }
  await page.goto(`${A}/set-cookie?next=/q3-frame-parent.html`);
  await expect
    .poll(() => page.evaluate(() => (window as unknown as { reports: unknown[] }).reports.length))
    .toBeGreaterThan(0);
  const [report] = await page.evaluate(
    () =>
      (window as unknown as { reports: { origin: string; data: { ran: string[]; origin: string; cookie: string } }[] })
        .reports,
  );
  const tags = ["q3f-kit-classic", "q3f-main", "q3f-other", "q3f-kit-module-noacao", "q3f-report"];
  const findings = {
    messageOrigin: report!.origin,
    frameOrigin: report!.data.origin,
    documentCookie: report!.data.cookie,
    ran: report!.data.ran,
    requests: Object.fromEntries(tags.map((t) => [t, seen(spike.tagged(t))])),
  };
  await record(info, "q3-frame", `${browserName} ${browser.version()}`, findings);

  // The frame is opaque, so its messages come from "null".
  expect(findings.messageOrigin).toBe("null");
  expect(findings.frameOrigin).toBe("null");
  // Controls: the kit's classic script and the app's own module ran.
  expect(findings.ran).toContain("/kit/k.js");
  expect(findings.ran).toContain("/apps/demo/~cap/main.js");
  // Another cap's path is outside script-src: refused before it is fetched.
  expect(findings.ran).not.toContain("/apps/demo/~other/evil.js");
  expect(findings.requests["q3f-other"]!.requests).toBe(0);
  // A module from origin null without ACAO fails CORS, which is why §4.2
  // sends `Access-Control-Allow-Origin: *` on JS.
  expect(findings.ran).not.toContain("/kit/m.js");
  // A module load from an opaque origin is a cross-origin CORS request in
  // "same-origin" credentials mode: no cookie, in either engine.
  expect(findings.requests["q3f-main"]!.strict).toBe(false);
  expect(findings.requests["q3f-main"]!.lax).toBe(false);
  expect(findings.requests["q3f-main"]!.origin).toBe("null");
  // A classic (no-cors) script from the kit, by contrast, is the Q1 answer
  // again: the cookie goes with it on WebKit and not on Chromium.
  const engine = isWebKit(browserName) ? EXPECTED.webkit : EXPECTED.chromium;
  expect(findings.requests["q3f-kit-classic"]!.requests).toBeGreaterThan(0);
  expect(findings.requests["q3f-kit-classic"]!.strict).toBe(engine.q3fStrictOnKitClassic);
});

test("Q4: Origin on a WebSocket opened by an opaque-origin frame", async ({
  page,
  spike,
  browser,
  browserName,
}, info) => {
  spike.headers("/q4-child-csp.html", {
    "content-security-policy": "sandbox allow-scripts allow-forms; connect-src 'none'",
  });
  await page.goto(`${spike.A}/set-cookie?next=/q4-parent.html`);
  await expect.poll(() => spike.tagged("q4-parent").length).toBeGreaterThan(0);
  await expect.poll(() => spike.tagged("q4-child").length).toBeGreaterThan(0);
  // The CSP child loaded; give a socket that was not refused time to arrive.
  await expect.poll(() => spike.tagged("q4-csp-doc").length).toBeGreaterThan(0);
  await page.waitForTimeout(1_000);
  const socket = (tag: string) => {
    const h = spike.tagged(tag).find((x) => x.websocket);
    return h ? seen([h]) : null;
  };
  const findings = {
    parent: socket("q4-parent"),
    sandboxedFrame: socket("q4-child"),
    sandboxedFrameUnderConnectSrcNone: socket("q4-csp"),
  };
  await record(info, "q4", `${browserName} ${browser.version()}`, findings);

  // Control: the parent's own socket carries its real origin.
  expect(findings.parent?.origin).toBe(spike.A);
  // The answer: the frame's socket says "null", which is what the core's
  // /ws check refuses (§4.9).
  expect(findings.sandboxedFrame?.origin).toBe("null");
  // And under `connect-src 'none'` it never reaches the server at all.
  expect(findings.sandboxedFrameUnderConnectSrcNone).toBeNull();
  const engine = isWebKit(browserName) ? EXPECTED.webkit : EXPECTED.chromium;
  // The same question as Q1, asked of a socket.
  expect(findings.sandboxedFrame?.strict).toBe(engine.q4StrictOnSandboxedSocket);
  // Fetch metadata on a WebSocket handshake. This decides whether §4.9's
  // "refuse /ws when Sec-Fetch-Site is missing" can ship as written: an
  // engine that never sends it on a handshake would be refused on the
  // shell's own socket.
  expect(findings.parent?.site).toBe(engine.q4SecFetchSiteOnParentSocket);
  expect(findings.sandboxedFrame?.site).toBe(engine.q4SecFetchSiteOnSandboxedSocket);
});

test("Q5: the Vue production build has no inline script and runs under the shell CSP", async ({
  page,
  spike,
  browser,
  browserName,
}, info) => {
  // Statically: every HTML file the build emits.
  const dist = join(WEB, "dist");
  const html = walk(dist).filter((f) => f.endsWith(".html"));
  const inline: { file: string; snippet: string }[] = [];
  for (const file of html) {
    const text = readFileSync(file, "utf8");
    for (const m of text.matchAll(/<script\b([^>]*)>([\s\S]*?)<\/script>/gi)) {
      if (!/\bsrc\s*=/.test(m[1]!) || m[2]!.trim() !== "") {
        inline.push({ file: file.slice(dist.length), snippet: m[0].slice(0, 120) });
      }
    }
    for (const m of text.matchAll(/\son[a-z]+\s*=\s*["']?[^"'\s>]/gi)) {
      inline.push({ file: file.slice(dist.length), snippet: `event handler attribute: ${m[0].trim()}` });
    }
    for (const m of text.matchAll(/javascript:/gi)) {
      inline.push({ file: file.slice(dist.length), snippet: `javascript: URL at ${m.index}` });
    }
  }

  // Empirically: the §4.10 CSP (see `shellCsp`).
  const csp = (o: string, tag: string) => shellCsp(o, `${spike.A}/csp-report?tag=${tag}`);
  const reports = (tag: string) => spike.tagged(tag).filter((h) => h.method === "POST");

  // Control: an inline script under the same policy is refused and reported.
  spike.headers("/q5-control.html", { "content-security-policy": csp(spike.A, "q5-control") });
  await page.goto(`${spike.A}/q5-control.html`);
  await expect.poll(() => reports("q5-control").length).toBeGreaterThan(0);
  await expect(page).toHaveTitle("untouched");

  // The real build, served by B at its root.
  const errors: string[] = [];
  page.on("console", (m) => {
    if (m.type() === "error" && /content security policy|refused to/i.test(m.text())) errors.push(m.text());
  });
  for (const path of ["/", "/index.html"]) {
    spike.headers(path, { "content-security-policy": csp(spike.B, "q5-shell") });
  }
  await page.goto(`${spike.B}/`);
  // The shell mounted: Vue ran from /assets/ under a policy with no inline.
  await expect(page.locator(".shell")).toBeAttached();
  await page.waitForTimeout(1_500);

  const findings = {
    htmlFiles: html.map((f) => f.slice(dist.length)),
    inline,
    shellMounted: true,
    cspReports: reports("q5-shell").map((h) => h.body.slice(0, 500)),
    cspConsoleErrors: errors,
  };
  await record(info, "q5", `${browserName} ${browser.version()}`, findings);

  expect(html).toContain(join(dist, "index.html"));
  expect(inline).toEqual([]);
  expect(findings.cspReports).toEqual([]);
  expect(errors).toEqual([]);
});

test("Q5: what Quick Look's PDF viewer needs that the §4.10 shell CSP refuses", async ({
  page,
  spike,
  browser,
  browserName,
}, info) => {
  // The Q5 test above only mounts the shell. Quick Look's PDF viewer
  // (src/shell/pdf.ts) also starts pdf.js's module worker from /assets/,
  // points pdf.js at WebAssembly decoders, and leaves pdf.js's eval probe on.
  // Each is asked here, from the real build's page under the real policy, in
  // the way pdf.js asks it.
  const worker = readdirSync(join(WEB, "dist", "assets")).find((f) => /^pdf\.worker.*\.mjs$/.test(f));
  expect(worker, "the build ships pdf.js's worker").toBeTruthy();
  const workerPath = `/assets/${worker}`;
  const policy = shellCsp(spike.B, `${spike.A}/csp-report?tag=q5-pdf`);

  // Control: the same policy with /assets/ added to worker-src. The worker
  // is fetched and runs, so a refusal below is the policy, not a broken
  // worker or a probe that cannot see one start.
  spike.headers("/index.html", {
    "content-security-policy": shellCsp(spike.B, `${spike.A}/csp-report?tag=q5-pdf-control`).replace(
      `worker-src ${spike.B}/sw.js`,
      `worker-src ${spike.B}/sw.js ${spike.B}/assets/`,
    ),
  });
  await page.goto(`${spike.B}/index.html`);
  await expect(page.locator(".shell")).toBeAttached();
  const control = await page.evaluate(pdfProbe, workerPath);
  const controlFetched = spike.hits.filter((h) => h.server === "B" && h.path === workerPath).length;

  // The answer: §4.10 exactly as written.
  spike.headers("/", { "content-security-policy": policy });
  await page.goto(`${spike.B}/`);
  await expect(page.locator(".shell")).toBeAttached();
  const probe = await page.evaluate(pdfProbe, workerPath);
  await expect.poll(() => spike.tagged("q5-pdf").filter((h) => h.method === "POST").length).toBeGreaterThan(0);
  const findings = {
    control: { ...control, workerFetched: controlFetched > 0 },
    ...probe,
    workerFetched: spike.hits.filter((h) => h.server === "B" && h.path === workerPath).length > controlFetched,
    cspReports: spike
      .tagged("q5-pdf")
      .filter((h) => h.method === "POST")
      .map((h) => h.body.slice(0, 300)),
  };
  await record(info, "q5-pdf", `${browserName} ${browser.version()}`, findings);

  expect(findings.control.workerFetched).toBe(true);
  expect(findings.control.worker).toBe("ran");
  expect(findings.control.violations).not.toContain("worker-src");

  // The answer, the same in both engines: §4.10 as written refuses
  // pdf.js's worker, before it is fetched.
  expect(findings.worker).not.toBe("ran");
  expect(findings.workerFetched).toBe(false);
  expect(findings.violations).toContain("worker-src");
  const engine = isWebKit(browserName) ? EXPECTED.webkit : EXPECTED.chromium;
  expect(findings.wasm).toBe(engine.q5Wasm);
  expect(findings.eval).toBe(engine.q5Eval);
});

/**
 * Asked inside the shell's page, the way pdf.js asks: start its module
 * worker, compile WebAssembly, and probe for eval. Runs in the browser, so it
 * may use nothing from this file.
 */
async function pdfProbe(workerPath: string) {
  const violations: string[] = [];
  document.addEventListener("securitypolicyviolation", (e) => violations.push(e.effectiveDirective));
  const worker = await new Promise<string>((done) => {
    try {
      const w = new Worker(workerPath, { type: "module" });
      w.addEventListener("error", () => done("error event"));
      // pdf.js's worker announces itself once it runs; any message means it ran.
      w.addEventListener("message", () => done("ran"));
      setTimeout(() => done("no answer"), 3_000);
    } catch (error) {
      done(`threw ${(error as Error).name}`);
    }
  });
  let wasm: string;
  try {
    // The smallest valid module: magic and version.
    await WebAssembly.compile(new Uint8Array([0, 0x61, 0x73, 0x6d, 1, 0, 0, 0]));
    wasm = "compiled";
  } catch (error) {
    wasm = `refused ${(error as Error).name}`;
  }
  let evalProbe: string;
  try {
    new Function("");
    evalProbe = "allowed";
  } catch (error) {
    evalProbe = `refused ${(error as Error).name}`;
  }
  await new Promise((r) => setTimeout(r, 300));
  return { worker, wasm, eval: evalProbe, violations };
}

/**
 * The shell's policy from §4.10, with {O} = `o`, `ws:` for `wss:` (the spike
 * is plain HTTP), and a report-uri so a refusal is seen by the server and not
 * only in a console.
 */
function shellCsp(o: string, reportUri: string): string {
  return (
    `default-src 'self'; script-src ${o}/assets/ ${o}/kit/; worker-src ${o}/sw.js; ` +
    `style-src 'self' 'unsafe-inline'; connect-src 'self' ${o.replace(/^http/, "ws")}; ` +
    `frame-src ${o}/apps/ ${o}/content; img-src 'self' data: blob:; object-src 'none'; ` +
    `base-uri 'none'; form-action 'self'; frame-ancestors 'none'; report-uri ${reportUri}`
  );
}

function walk(dir: string): string[] {
  return readdirSync(dir).flatMap((name) => {
    const path = join(dir, name);
    return statSync(path).isDirectory() ? walk(path) : [path];
  });
}

/**
 * The answers found, per engine, where the assertion depends on the engine.
 * From the run of 2026-11-16: Chromium 156.0.8078.4 and WebKit 27.2
 * (Playwright 1.64.1; WPE on Linux, so libsoup networking, not Safari's).
 * Recorded in docs/APP-UI-ISOLATION.md §8.
 */
const EXPECTED = {
  chromium: {
    // Withholds SameSite cookies from everything a sandboxed frame sends.
    q1StrictFromSandbox: [] as string[],
    q2CrossReached: 0,
    q2PathReached: 0,
    q2FrameSrcViolations: 2,
    q4StrictOnSandboxedSocket: false,
    // No fetch metadata at all on a WebSocket handshake.
    q4SecFetchSiteOnParentSocket: null as string | null,
    q4SecFetchSiteOnSandboxedSocket: null as string | null,
    q3fStrictOnKitClassic: false,
    q5Wasm: "refused CompileError",
    q5Eval: "refused EvalError",
  },
  webkit: {
    // Sends SameSite=Strict (and Lax) cookies on every request a sandboxed
    // frame makes to the parent's origin, while labeling each cross-site.
    q1StrictFromSandbox: ["img", "script", "style", "fetch-nocors", "fetch-cors", "nav"],
    q2CrossReached: 0,
    q2PathReached: 0,
    q2FrameSrcViolations: 2,
    q4StrictOnSandboxedSocket: true,
    q4SecFetchSiteOnParentSocket: "same-origin" as string | null,
    q4SecFetchSiteOnSandboxedSocket: "cross-site" as string | null,
    q3fStrictOnKitClassic: true,
    q5Wasm: "refused CompileError",
    q5Eval: "refused EvalError",
  },
};
