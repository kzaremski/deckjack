// The Photos app's entry point: real dependencies into `mountPhoto`.
// A file, not an inline script, because the brokered frame's CSP has no
// 'unsafe-inline' for scripts (APP-UI-ISOLATION §4.4).

import { bus } from "/kit/bus.js";
import { shell } from "/kit/shell.js";
import { createRenderer, decodeProxy } from "./gl.js";
import { mountPhoto } from "./photo.js";

const app = mountPhoto(document, { bus, shell, createRenderer, decodeProxy });
app.start(new URLSearchParams(location.search).get("path")).catch((e) => {
  document.getElementById("status").textContent = e instanceof Error ? e.message : String(e);
});
