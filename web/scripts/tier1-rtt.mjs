// The UI protocol's event round trip, against a real node over a real
// network: docs/lanes/9-tier1-phase0.md, "Pending: the phone over the
// tailnet".
//
// Two ways to run it, for the two halves of the question.
//
// 1. From a laptop over the tailnet (this script, with Playwright):
//
//      DJ_NODE=https://<node>.<tailnet>.ts.net \
//      DJ_COOKIE='__Host-dj_session=<value>' \
//      node web/scripts/tier1-rtt.mjs [samples]
//
//    DJ_COOKIE is a signed-in session (copy it from the browser's storage
//    panel). Tasks must be installed and approved on that node. The device
//    signs in as "tier1-rtt" and so takes the single seat; take it back from
//    the phone or desktop afterwards. Prints p50/p95/max of the round trip
//    (event sent to the app's acknowledging frame back) in milliseconds.
//
// 2. On the phone itself, in its own browser, with no tooling: open the
//    shell, then in the address bar or a bookmark run
//
//      javascript:localStorage.setItem("deckjack.uiStats","1");location.reload()
//
//    open Tasks, tick "Show done" 40 times, and run
//
//      javascript:(s=>alert(JSON.stringify(((a)=>{a=[...a].sort((x,y)=>x-y);
//        const p=q=>a[Math.min(a.length-1,Math.ceil(q*a.length)-1)];
//        return{n:a.length,p50:p(.5),p95:p(.95),max:a.at(-1)}})(s.rtt))))(__djUiStats)
//
//    which shows the same three numbers, measured by the phone's own clock,
//    over the phone's own radio. Clear it with
//    `localStorage.removeItem("deckjack.uiStats")`.
import { createRequire } from "node:module";

const require = createRequire(new URL("../package.json", import.meta.url));
const { chromium } = require("@playwright/test");

const origin = process.env.DJ_NODE;
const cookie = process.env.DJ_COOKIE;
const samples = Number(process.argv[2] ?? 40);
if (!origin || !cookie || !cookie.includes("=")) {
  console.error("set DJ_NODE=<origin> and DJ_COOKIE=<name>=<value>; see the comment at the top");
  process.exit(2);
}
const [name, ...rest] = cookie.split("=");
const url = new URL(origin);

const browser = await chromium.launch();
try {
  const context = await browser.newContext({ serviceWorkers: "block" });
  await context.addCookies([
    {
      name,
      value: rest.join("="),
      domain: url.hostname,
      path: "/",
      httpOnly: true,
      secure: url.protocol === "https:",
      sameSite: "Strict",
    },
  ]);
  await context.addInitScript(() => {
    localStorage.setItem("deckjack.uiStats", "1");
    localStorage.setItem("deckjack.device", "tier1-rtt");
  });
  const page = await context.newPage();
  await page.goto(new URL("/?open=tasks", origin).toString());
  const panel = page.locator('.ui-panel[data-app="tasks"]');
  const box = panel.getByRole("checkbox", { name: "Show done" });
  await box.waitFor({ timeout: 60_000 });
  const count = () => page.evaluate(() => globalThis.__djUiStats.rtt.length);
  for (let i = 0; i < samples; i++) {
    const before = await count();
    await box.click();
    const deadline = Date.now() + 10_000;
    while ((await count()) === before) {
      if (Date.now() > deadline) throw new Error("no acknowledgement within 10 s");
      await page.waitForTimeout(5);
    }
  }
  const rtt = await page.evaluate(() => [...globalThis.__djUiStats.rtt]);
  const sorted = rtt.slice(-samples).sort((a, b) => a - b);
  const p = (q) => sorted[Math.min(sorted.length - 1, Math.ceil(q * sorted.length) - 1)];
  console.log(
    JSON.stringify({ origin, n: sorted.length, p50: p(0.5), p95: p(0.95), max: sorted.at(-1) }, null, 2),
  );
} finally {
  await browser.close();
}
