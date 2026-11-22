/**
 * Production wiring for the Office UI: the kit's bus and shell, this
 * device's storage, the real window. A file of its own rather than an inline
 * script, because the isolated frame's CSP allows no inline script
 * (docs/APP-UI-ISOLATION.md §4.4).
 */
import { bus } from "/kit/bus.js";
import { shell } from "/kit/shell.js";
import { mountOffice } from "./office.js";

// `localStorage` throws outright in some private windows; Office is handed
// something that never does, and works without it.
const storage = {
  read(key) {
    try {
      return localStorage.getItem(key);
    } catch {
      return null;
    }
  },
  write(key, value) {
    try {
      localStorage.setItem(key, value);
      return true;
    } catch {
      return false;
    }
  },
};

const query = new URLSearchParams(location.search);
const office = mountOffice(document, {
  bus,
  shell,
  storage,
  win: window,
});
office.start(query.get("path")).catch((e) => {
  document.getElementById("status").textContent = e instanceof Error ? e.message : String(e);
});
