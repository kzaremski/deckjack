/*
 * The shell's offline cache.
 *
 * It caches the shell and nothing else. Everything behind the command bus is
 * server state, and a stale window list or a cached file listing would be
 * worse than an error: connected clients hold no state of their own, and the
 * running server is the single source of truth.
 *
 * Plain JavaScript on purpose. It is served as a static file from the
 * embedded bundle, so it is never built, bundled or transpiled, and it has no
 * imports that a build step would have to resolve.
 */

const VERSION = "deckjack-shell-v1";

/** Fetched on install so a cold offline launch has something to open. */
const SHELL = ["/", "/manifest.webmanifest", "/icons/deckjack.svg"];

/** Never cached: sign-in, the bus, app routes, anything that is state. */
const NEVER = [/^\/api\//, /^\/ws$/, /^\/login/, /^\/logout/, /^\/apps\//];

self.addEventListener("install", (event) => {
  event.waitUntil(
    caches
      .open(VERSION)
      .then((cache) => cache.addAll(SHELL))
      // A missing file must not leave a half-installed worker behind.
      .catch(() => undefined)
      .then(() => self.skipWaiting()),
  );
});

self.addEventListener("activate", (event) => {
  event.waitUntil(
    caches
      .keys()
      .then((keys) => Promise.all(keys.filter((k) => k !== VERSION).map((k) => caches.delete(k))))
      .then(() => self.clients.claim()),
  );
});

self.addEventListener("fetch", (event) => {
  const request = event.request;
  if (request.method !== "GET") return;

  const url = new URL(request.url);
  if (url.origin !== self.location.origin) return;
  if (NEVER.some((pattern) => pattern.test(url.pathname))) return;

  // Hashed assets are immutable: a new build has new names, so a hit is
  // always correct and a miss is always worth storing.
  if (url.pathname.startsWith("/assets/")) {
    event.respondWith(
      caches.match(request).then(
        (hit) =>
          hit ||
          fetch(request).then((response) => {
            if (response.ok) {
              const copy = response.clone();
              caches.open(VERSION).then((cache) => cache.put(request, copy));
            }
            return response;
          }),
      ),
    );
    return;
  }

  // Navigations go to the network first so a new shell is picked up as soon
  // as it exists, and fall back to the cached shell when the server is gone.
  if (request.mode === "navigate") {
    event.respondWith(
      fetch(request)
        .then((response) => {
          const copy = response.clone();
          caches.open(VERSION).then((cache) => cache.put("/", copy));
          return response;
        })
        .catch(() => caches.match("/").then((hit) => hit || Response.error())),
    );
  }
});
