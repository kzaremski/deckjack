import { createApp } from "vue";

import App from "./App.vue";
import { createShell } from "./shell/context.ts";
import { isAppId } from "./stores/apps.ts";
import "./styles/base.css";

/**
 * The entry point.
 *
 * The stores are built before the app is created and handed in as a prop, so
 * the component tree never constructs its own state and a second shell
 * (a popped-out window, a test harness) is a second `createShell()` rather
 * than a second module graph.
 */
const shell = createShell();

createApp(App, { shell }).mount("#app");

/**
 * PWA shortcuts land on `/?open=<app>`, which is the only route the shell
 * reads from the URL: opening a window is a command, so the query turns into
 * one dispatch and the parameter is dropped from the address bar rather than
 * becoming a second source of truth about what is open.
 */
const launch = new URLSearchParams(location.search).get("open");
if (launch && isAppId(launch)) {
  void shell.commands.dispatch(`app.open.${launch}`);
  history.replaceState(null, "", location.pathname);
}

/**
 * The service worker makes the installed PWA open without the network, which
 * matters most in the case the design doc names: the laptop hosting its own
 * server offline. It caches the shell, never data — anything behind the bus
 * is server state and must not be served stale.
 *
 * Registered after mount so a failing worker can never keep the shell from
 * appearing, and skipped in development where Vite serves the modules.
 */
if ("serviceWorker" in navigator && import.meta.env.PROD) {
  window.addEventListener("load", () => {
    navigator.serviceWorker.register("/sw.js").catch((error: unknown) => {
      console.warn("deckjack: the shell cache is unavailable", error);
    });
  });
}
