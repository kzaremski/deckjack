import type { BrowserContext, Page } from "@playwright/test";

/**
 * Network latency on the shell's socket, without `tc` or root: Playwright
 * stands between the page and the node (`routeWebSocket`), and every frame
 * waits `oneWayMs` before it is passed on, in either direction. So an
 * injected round trip is `2 * oneWayMs`, on top of whatever the loopback
 * costs. Order is kept: every frame waits the same time, and timers with
 * equal delays fire in the order they were set.
 *
 * The counters are how a test proves something did *not* use the network:
 * they count the shell's UI protocol frames each way, by what the shell
 * actually sent and what the node actually sent, and a test reads them before
 * and after an interaction.
 */
export interface Wire {
  /** UI protocol frames the shell sent. */
  up: number;
  /** UI protocol frames the node sent. */
  down: number;
  /** Change the injected delay for frames from now on. */
  setDelay(oneWayMs: number): void;
}

export async function delaySocket(target: Page | BrowserContext, oneWayMs: number): Promise<Wire> {
  let delay = oneWayMs;
  const wire: Wire = {
    up: 0,
    down: 0,
    setDelay(ms) {
      delay = ms;
    },
  };
  const isUi = (message: string | Buffer) =>
    typeof message === "string" && message.startsWith('{"type":"ui"');
  await target.routeWebSocket(/\/ws$/, (ws) => {
    const server = ws.connectToServer();
    ws.onMessage((message) => {
      if (isUi(message)) wire.up += 1;
      const send = () => server.send(message);
      if (delay > 0) setTimeout(send, delay);
      else send();
    });
    server.onMessage((message) => {
      if (isUi(message)) wire.down += 1;
      const send = () => ws.send(message);
      if (delay > 0) setTimeout(send, delay);
      else send();
    });
    ws.onClose((code, reason) => server.close({ code, reason }));
    server.onClose((code, reason) => ws.close({ code, reason }));
  });
  return wire;
}
