import { createHash } from "node:crypto";
import { existsSync, readFileSync, statSync } from "node:fs";
import { createServer, type IncomingMessage, type Server, type ServerResponse } from "node:http";
import type { AddressInfo } from "node:net";
import type { Duplex } from "node:stream";
import { extname, join, normalize, resolve } from "node:path";
import { fileURLToPath } from "node:url";

/**
 * The spike's server: a few static pages and a request log, nothing else.
 *
 * Not the core, on purpose. The questions in docs/APP-UI-ISOLATION.md §8
 * are about what browsers do, and the answers must not depend on what the
 * core happens to do today. Two listeners make two sites:
 *
 * - **A**, `http://127.0.0.1:<port>`: plays the Deckjack origin. It sets the
 *   session-like cookies and serves the shell-like parent pages.
 * - **B**, `http://localhost:<port>`: somewhere else. A different host is a
 *   different site, so anything that reaches B has left.
 *
 * Every request either server sees is logged with the headers the questions
 * turn on (Cookie, Origin, Sec-Fetch-*), and a WebSocket upgrade is answered
 * by hand (RFC 6455 handshake, one text frame, close) so no dependency is
 * needed. Pages are `pages/*.html` with `{{A}}` and `{{B}}` replaced by the
 * two origins, and served with whatever headers the test registered for them.
 */

const HERE = resolve(fileURLToPath(new URL(".", import.meta.url)));
const PAGES = join(HERE, "pages");
const DIST = resolve(HERE, "..", "..", "dist");

export interface Hit {
  server: "A" | "B";
  method: string;
  path: string;
  /** The query's `tag`, which every probe request carries. */
  tag: string | null;
  cookie: string | null;
  origin: string | null;
  secFetchSite: string | null;
  secFetchDest: string | null;
  secFetchMode: string | null;
  websocket: boolean;
  body: string;
}

export interface Spike {
  A: string;
  B: string;
  hits: Hit[];
  /** Hits whose tag is `tag`. */
  tagged(tag: string): Hit[];
  /** Serve `path` (exact, without the query) with these extra headers. */
  headers(path: string, headers: Record<string, string>): void;
  close(): Promise<void>;
}

const TYPES: Record<string, string> = {
  ".html": "text/html; charset=utf-8",
  ".js": "text/javascript; charset=utf-8",
  ".mjs": "text/javascript; charset=utf-8",
  ".css": "text/css; charset=utf-8",
  ".json": "application/json",
  ".svg": "image/svg+xml",
  ".png": "image/png",
  ".webmanifest": "application/manifest+json",
  ".gif": "image/gif",
  ".woff2": "font/woff2",
  ".map": "application/json",
};

/** A 1x1 transparent GIF, for probes loaded as images. */
const GIF = Buffer.from("R0lGODlhAQABAIAAAAAAAP///yH5BAEAAAAALAAAAAABAAEAAAIBRAA7", "base64");

export async function startSpike(): Promise<Spike> {
  const hits: Hit[] = [];
  const extra = new Map<string, Record<string, string>>();
  const origins = { A: "", B: "" };

  const handler = (server: "A" | "B") => async (req: IncomingMessage, res: ServerResponse) => {
    const url = new URL(req.url ?? "/", "http://x");
    const body = await readBody(req);
    hits.push(record(server, req, url, false, body));
    try {
      route(server, url, res);
    } catch (error) {
      res.writeHead(500, { "content-type": "text/plain" });
      res.end(String(error));
    }
  };

  function route(server: "A" | "B", url: URL, res: ServerResponse) {
    const path = url.pathname;
    const headers: Record<string, string> = { "cache-control": "no-store", ...(extra.get(path) ?? {}) };

    // A top-level navigation here sets the cookies, then goes on. Strict and
    // Lax both, because the two differ exactly on cross-site navigations.
    if (path === "/set-cookie") {
      res.writeHead(303, {
        ...headers,
        "set-cookie": [
          "strict=1; SameSite=Strict; Path=/; HttpOnly",
          "lax=1; SameSite=Lax; Path=/; HttpOnly",
        ],
        location: url.searchParams.get("next") ?? "/",
      });
      res.end();
      return;
    }

    // Probes: anything under /probe/ answers with the type the extension
    // asks for, so one route serves images, scripts, styles and pages.
    if (path.includes("/probe/")) {
      const ext = extname(path);
      if (ext === ".js") {
        res.writeHead(200, { ...headers, "content-type": TYPES[".js"]! });
        res.end(`/* probe ${url.searchParams.get("tag")} */`);
      } else if (ext === ".css") {
        res.writeHead(200, { ...headers, "content-type": TYPES[".css"]! });
        res.end("/* probe */");
      } else if (ext === ".html") {
        res.writeHead(200, { ...headers, "content-type": TYPES[".html"]! });
        res.end(`<!doctype html><title>landed ${url.searchParams.get("tag")}</title><p>landed</p>`);
      } else {
        res.writeHead(200, { ...headers, "content-type": "image/gif" });
        res.end(GIF);
      }
      return;
    }

    // B serves the Vue production build at its root, for question 5: the
    // build's URLs are absolute (`/assets/...`), so it cannot sit under a
    // prefix. A keeps the pages, whose `/kit/` is the spike's own.
    if (server === "B") {
      serveFile(res, DIST, path === "/" ? "/index.html" : path, headers, false);
      return;
    }

    // Everything else on A is a page or script from pages/.
    serveFile(res, PAGES, path, headers, true);
  }

  function serveFile(
    res: ServerResponse,
    root: string,
    path: string,
    headers: Record<string, string>,
    template: boolean,
  ) {
    const file = normalize(join(root, decodeURIComponent(path)));
    if (!file.startsWith(root) || !existsSync(file) || !statSync(file).isFile()) {
      res.writeHead(404, { ...headers, "content-type": "text/plain" });
      res.end("not found");
      return;
    }
    const type = TYPES[extname(file)] ?? "application/octet-stream";
    let bytes: Buffer | string = readFileSync(file);
    if (template && /^text\/(html|javascript)/.test(type)) {
      bytes = bytes.toString("utf8").replaceAll("{{A}}", origins.A).replaceAll("{{B}}", origins.B);
    }
    res.writeHead(200, { "content-type": type, ...headers });
    res.end(bytes);
  }

  /** RFC 6455: accept, say one thing, close. Enough to see the request. */
  const upgrade = (server: "A" | "B") => (req: IncomingMessage, socket: Duplex) => {
    const url = new URL(req.url ?? "/", "http://x");
    hits.push(record(server, req, url, true, ""));
    const key = req.headers["sec-websocket-key"];
    if (typeof key !== "string") {
      socket.end("HTTP/1.1 400 Bad Request\r\n\r\n");
      return;
    }
    const accept = createHash("sha1")
      .update(key + "258EAFA5-E914-47DA-95CA-C5AB0DC85B11")
      .digest("base64");
    socket.write(
      "HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\n" +
        `Sec-WebSocket-Accept: ${accept}\r\n\r\n`,
    );
    const text = Buffer.from("hello");
    socket.write(Buffer.concat([Buffer.from([0x81, text.length]), text]));
    socket.on("error", () => {});
    socket.on("data", () => {});
    // An upgraded socket is no longer the HTTP server's to close, so it is
    // tracked here; otherwise `close` waits on it forever.
    upgraded.add(socket);
    socket.on("close", () => upgraded.delete(socket));
  };
  const upgraded = new Set<Duplex>();

  const servers: Server[] = [];
  for (const [name, host] of [
    ["A", "127.0.0.1"],
    ["B", "localhost"],
  ] as const) {
    const server = createServer(handler(name));
    server.on("upgrade", upgrade(name));
    await new Promise<void>((ok) => server.listen(0, "127.0.0.1", ok));
    const { port } = server.address() as AddressInfo;
    origins[name] = `http://${host}:${port}`;
    servers.push(server);
  }

  return {
    A: origins.A,
    B: origins.B,
    hits,
    tagged: (tag) => hits.filter((h) => h.tag === tag),
    headers: (path, h) => void extra.set(path, h),
    close: async () => {
      for (const socket of upgraded) socket.destroy();
      for (const s of servers) {
        s.closeAllConnections();
        await new Promise<void>((ok) => s.close(() => ok()));
      }
    },
  };
}

function record(server: "A" | "B", req: IncomingMessage, url: URL, websocket: boolean, body: string): Hit {
  const h = (name: string) => {
    const v = req.headers[name];
    return typeof v === "string" ? v : null;
  };
  return {
    server,
    method: req.method ?? "GET",
    path: url.pathname,
    tag: url.searchParams.get("tag"),
    cookie: h("cookie"),
    origin: h("origin"),
    secFetchSite: h("sec-fetch-site"),
    secFetchDest: h("sec-fetch-dest"),
    secFetchMode: h("sec-fetch-mode"),
    websocket,
    body,
  };
}

function readBody(req: IncomingMessage): Promise<string> {
  if (req.method === "GET" || req.method === "HEAD") return Promise.resolve("");
  return new Promise((ok) => {
    let text = "";
    req.on("data", (c: Buffer) => (text += c.toString()));
    req.on("end", () => ok(text));
    req.on("error", () => ok(text));
  });
}
