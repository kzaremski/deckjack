/**
 * The Files app's UI.
 *
 * A module that takes everything it touches as an argument -- the bus, the
 * shell bridge, the selection model, the clock, storage, hit-testing -- so
 * `index.html` mounts it with the real kit and a test mounts it with fakes.
 * Nothing here reaches for a global except the DOM it is handed and, at the
 * moment it is used, `window.deckjackShell` (see "Open with" below).
 *
 * The interaction model, desktop and touch, because the plan holds this lane
 * to full parity on the phone:
 *
 * - **Pointer:** click selects, Ctrl/Cmd-click toggles, Shift-click extends,
 *   double-click opens, right-click is the context menu, and dragging rows
 *   onto a folder (or a breadcrumb) moves them there.
 * - **Touch:** tap opens; long-press enters selection mode with that row
 *   selected, and in selection mode a tap toggles. Selection is a mode, not a
 *   modifier, because a phone has no modifier keys. A long-press that then
 *   moves without lifting is a drag: drop on a folder to move into it.
 * - **Keyboard:** arrows move (Shift extends), Home/End, Enter opens, Space
 *   previews, Ctrl/Cmd+A selects all, F2 renames, Escape clears, `/` filters,
 *   the context-menu key (or Shift+F10) opens the menu, and Backspace or
 *   Alt+Up goes up a folder.
 *
 * "Open" means: a folder is entered; a text file goes to the text editor
 * when the desktop can open one, and anything else (or a text file when it
 * cannot) goes to Quick Look with the rest of the folder's files to step
 * through. The shell owns Quick Look and app windows; this app asks for them
 * through `/kit/shell.js` and `window.deckjackShell`, and never assumes the
 * answer is yes.
 *
 * Moving and copying are Cut/Copy then Paste, or a drag. Paste into the
 * folder a copy came from makes "name copy"; a paste that meets a name
 * already taken offers Keep both. Deleting goes to the trash and offers Undo;
 * the trash is a view of its own with Restore. Nothing here deletes for good:
 * emptying the trash is a destructive command, and a destructive command is
 * confirmed by the shell's own dialog, which an app's frame cannot draw or
 * answer. It is in the palette.
 *
 * Rows are drawn when the listing, the sort or the filter changes, and only
 * their selected state is touched when the selection changes. That is not an
 * optimization: a thumbnail that reloaded on every click would flicker, and a
 * touch drag whose row was replaced under the finger would lose its events.
 */

/** How long a press must be held to count as a long-press. */
export const LONG_PRESS_MS = 500;
/** How far a finger may drift before a press becomes a scroll. */
const LONG_PRESS_SLOP = 10;
/** How far a mouse must move with the button down before it is a drag. */
const DRAG_SLOP = 5;
/** Thumbnails asked for at once. The core decodes a few at a time anyway. */
export const THUMBNAIL_CONCURRENCY = 3;
/** The edge a row's thumbnail is asked for at: rows are small. */
export const THUMBNAIL_SIZE = 128;
/** Below this much room, the free-space line says so. */
const LOW_SPACE_BYTES = 1024 ** 3;
const LOW_SPACE_FRACTION = 0.05;

const IMAGE = /\.(jpe?g|png|gif|webp|bmp)$/i;
const TEXT =
  /\.(txt|md|markdown|rst|org|json|jsonc|toml|ya?ml|ini|cfg|conf|env|log|csv|tsv|xml|html?|css|scss|less|js|mjs|cjs|jsx|ts|tsx|vue|svelte|rs|py|rb|go|java|kt|swift|c|h|cc|cpp|hpp|cs|php|lua|pl|sh|bash|zsh|fish|ps1|sql|graphql|proto|lock|diff|patch|tex|nix|dockerfile|gitignore|editorconfig)$/i;
const TEXT_NAMES = /^(readme|license|licence|copying|changelog|makefile|dockerfile|justfile|procfile|authors|contributors|todo)$/i;

/** Whether a file opens in the text editor. */
export function isText(name) {
  return TEXT.test(name) || TEXT_NAMES.test(name);
}

/** Whether the core makes a thumbnail of a file with this name. */
export function isImage(name) {
  return IMAGE.test(name);
}

/** The Editor's app name: `scribe`, since `editor` is the core's namespace. */
const EDITOR = "scribe";

/** The apps "Open with" offers. Names are AppTable names. */
const OPEN_WITH = [{ app: EDITOR, label: "Text editor", fits: (e) => e.kind !== "dir" }];

const SORTS = ["name", "size", "modified", "kind"];
const SORT_KEY = "deckjack.files.sort";

/**
 * Addresses come back from the core qualified with the node (`r620:/a`).
 * Files works on this node only -- there is no transport to another node's
 * files yet -- so it keeps the local form, which the core also accepts.
 */
export function local(path) {
  return String(path).replace(/^[^/:]+:(?=\/)/, "");
}

function parentOf(path) {
  return path.slice(0, path.lastIndexOf("/")) || "/";
}

function nameOf(path) {
  return path.slice(path.lastIndexOf("/") + 1);
}

export function size(bytes) {
  if (bytes < 1024) return `${bytes} B`;
  const units = ["KB", "MB", "GB", "TB", "PB"];
  let value = bytes / 1024;
  let i = 0;
  while (value >= 1024 && i < units.length - 1) {
    value /= 1024;
    i++;
  }
  return `${value < 10 ? value.toFixed(1) : Math.round(value)} ${units[i]}`;
}

export function mountFiles(root, deps) {
  const { bus, shell, createSelection, text, failure } = deps;
  const delay =
    deps.delay ??
    ((ms, fn) => {
      const t = setTimeout(fn, ms);
      return () => clearTimeout(t);
    });

  // `root` is the document itself when the page mounts, and an element in a
  // test; `ownerDocument` is null on the first, so take whichever exists.
  const doc = root.ownerDocument ?? root;
  const win = doc.defaultView ?? globalThis;
  const hitTest = deps.hitTest ?? ((x, y) => doc.elementFromPoint?.(x, y) ?? null);
  const Observer =
    "IntersectionObserver" in deps ? deps.IntersectionObserver : win.IntersectionObserver;
  const storage = (() => {
    if ("storage" in deps) return deps.storage;
    try {
      return win.localStorage ?? null;
    } catch {
      return null;
    }
  })();

  const $ = (id) => root.querySelector(`#${id}`);
  const list = $("list");
  const status = $("status");
  const crumbs = $("crumbs");
  const selbar = $("selbar");
  const selcount = $("selcount");
  const undoButton = $("undo");
  const pasteButton = $("paste");
  const keepBothButton = $("keepboth");
  const filterInput = $("filter");
  const sortSelect = $("sort");
  const sortDir = $("sortdir");
  const spaceLine = $("space");
  const menu = $("menu");
  const ghost = $("ghost");

  /** Addresses are the core's namespace. `/` is the working directory. */
  let here = "/";
  /** "folder", or "trash" while the trash is shown in the list. */
  let view = "folder";
  /** What Cut or Copy picked up: carried across folders until pasted. */
  let clip = null;
  /** What the last trash can put back, until something else happens. */
  let undo = null;
  /** A copy refused for a taken name, which Keep both would retry. */
  let retry = null;
  /** The listing as it came, and as drawn (sorted, filtered). */
  let entries = [];
  let shown = [];
  const byPath = new Map();
  let filter = "";
  let sort = loadSort();
  /** The free space last reported, or null. */
  let space = null;

  const selection = createSelection({ onChange: drawState });

  function say(message) {
    status.textContent = message;
  }

  /** Offer Undo for a trash, or take the offer away. */
  function offerUndo(next) {
    undo = next;
    undoButton.hidden = !next;
  }

  function offerKeepBoth(next) {
    retry = next;
    keepBothButton.hidden = !next;
  }

  function child(name) {
    return here === "/" ? `/${name}` : `${here}/${name}`;
  }

  // -- the desktop's app windows --------------------------------------------

  /**
   * The shell's app opener, if this frame has one. Read when used, not at
   * mount: `/kit/shell.js` may define it after this module runs, and a frame
   * opened in a tab of its own never has one.
   */
  function opener() {
    const s = win.deckjackShell;
    return s && typeof s.openApp === "function" ? s : null;
  }

  /**
   * Whether each "Open with" app is installed on this desktop, as the shell
   * last said (`shell.canOpen`). Absent until it answers. An app the shell
   * would refuse is not offered: an item that does nothing when chosen is
   * worse than no item.
   */
  const installed = new Map();

  function checkOpeners() {
    const s = opener();
    if (!s || typeof s.canOpen !== "function") return;
    for (const { app } of OPEN_WITH) {
      Promise.resolve()
        .then(() => s.canOpen(app))
        .then(
          (yes) => installed.set(app, yes === true),
          () => installed.set(app, false),
        )
        .then(() => drawState());
    }
  }

  /** Offered in the menus: a desktop that says it has this app. A shell too
   * old to say is taken at its word when asked. */
  function offered(app) {
    const s = opener();
    if (!s) return false;
    if (typeof s.canOpen !== "function") return true;
    return installed.get(app) === true;
  }

  /**
   * Ask the desktop to open `path` in `app`. The shell answers, and a
   * refusal (the app is not installed, say) is reported with the shell's
   * reason. With `fallback`, a refused file is shown in Quick Look instead,
   * so "open" always opens something.
   */
  async function openWith(app, path, { fallback = false } = {}) {
    const s = opener();
    const label = OPEN_WITH.find((o) => o.app === app)?.label ?? app;
    const refused = (why) => {
      if (fallback) {
        peek(path);
        say(`${why} Showing it in Quick Look instead.`);
      } else {
        say(why);
      }
      return false;
    };
    if (!s) {
      say(`Opening files in the ${label.toLowerCase()} is part of the desktop; open Files from the dock to use it.`);
      return false;
    }
    let answer;
    try {
      answer = s.openApp(app, { path });
      if (answer === false) return refused(`The desktop did not open the ${label.toLowerCase()}.`);
      answer = await answer;
    } catch (e) {
      failure(status, e);
      if (fallback) peek(path);
      return false;
    }
    if (answer === true || answer?.ok === true) return true;
    // The shell said no; do not offer it again until it says otherwise.
    installed.set(app, false);
    drawState();
    return refused(answer?.reason || `The desktop did not open the ${label.toLowerCase()}.`);
  }

  // -- sorting and filtering ------------------------------------------------

  function loadSort() {
    try {
      const saved = JSON.parse(storage?.getItem(SORT_KEY) ?? "null");
      if (saved && SORTS.includes(saved.key) && (saved.dir === 1 || saved.dir === -1)) return saved;
    } catch {
      // Unreadable or blocked storage: the default is fine.
    }
    return { key: "name", dir: 1 };
  }

  function saveSort() {
    try {
      storage?.setItem(SORT_KEY, JSON.stringify(sort));
    } catch {
      // A private window: the choice lasts as long as the frame.
    }
  }

  const byName = (a, b) =>
    a.name.localeCompare(b.name, undefined, { numeric: true, sensitivity: "base" });
  const extOf = (e) => {
    const dot = e.name.lastIndexOf(".");
    return e.kind === "dir" || dot <= 0 ? "" : e.name.slice(dot + 1).toLowerCase();
  };

  /** Folders first, always, in either direction; then the chosen key. */
  function compare(a, b) {
    const folders = Number(b.kind === "dir") - Number(a.kind === "dir");
    if (folders) return folders;
    let order = 0;
    if (sort.key === "size") order = (a.size ?? 0) - (b.size ?? 0);
    else if (sort.key === "modified") {
      // Unknown times sort after known ones whichever way round.
      if (a.modified == null || b.modified == null) {
        const unknown = Number(a.modified == null) - Number(b.modified == null);
        if (unknown) return unknown;
      } else order = a.modified - b.modified;
    } else if (sort.key === "kind") order = extOf(a).localeCompare(extOf(b));
    return (order || byName(a, b)) * sort.dir;
  }

  function arrange() {
    const needle = filter.trim().toLowerCase();
    shown = entries
      .filter((e) => !needle || e.name.toLowerCase().includes(needle))
      .sort(view === "trash" ? () => 0 : compare);
  }

  function drawSortControls() {
    sortSelect.value = sort.key;
    const up = sort.dir === 1;
    sortDir.textContent = up ? "↑" : "↓";
    sortDir.setAttribute("aria-label", up ? "Ascending" : "Descending");
    sortDir.title = up ? "Ascending; click for descending" : "Descending; click for ascending";
  }

  // -- drawing --------------------------------------------------------------

  function drawCrumbs() {
    if (view === "trash") {
      crumbs.innerHTML =
        '<button class="crumb" data-path="/">Working directory</button>' +
        '<span class="sep" aria-hidden="true">/</span><button class="crumb" data-trash="">Trash</button>';
      return;
    }
    const parts = here.split("/").filter(Boolean);
    const items = [{ label: "Working directory", path: "/" }];
    let path = "";
    for (const part of parts) {
      path += `/${part}`;
      items.push({ label: part, path });
    }
    crumbs.innerHTML = items
      .map(
        (c, i) =>
          `${i ? '<span class="sep" aria-hidden="true">/</span>' : ""}` +
          `<button class="crumb" data-path="${text(c.path)}">${text(c.label)}</button>`,
      )
      .join("");
    // The breadcrumb scrolls rather than truncating, and it is scrolled to
    // the end so the folder you are in is the part you can see.
    crumbs.scrollLeft = crumbs.scrollWidth;
  }

  function when(seconds) {
    if (seconds == null) return "";
    const d = new Date(seconds * 1000);
    const pad = (n) => String(n).padStart(2, "0");
    return `${d.getFullYear()}-${pad(d.getMonth() + 1)}-${pad(d.getDate())} ${pad(d.getHours())}:${pad(d.getMinutes())}`;
  }

  function metaOf(e) {
    if (e.meta != null) return e.meta;
    if (sort.key === "modified") return when(e.modified);
    return e.kind === "dir" ? "" : size(e.size ?? 0);
  }

  /** The rows, from the listing. Then their state, then their thumbnails. */
  function drawRows() {
    if (shown.length === 0) {
      list.innerHTML = `<p class="dj-empty">${
        entries.length && filter ? `Nothing here matches “${text(filter)}”.` : "Nothing here."
      }</p>`;
    } else {
      list.innerHTML = shown
        .map((e) => {
          const thumb = view === "folder" && e.kind !== "dir" && !e.escapes && isImage(e.name);
          const icon = e.kind === "dir" ? "/" : e.escapes ? "!" : "";
          return `
          <div class="dj-row" role="option" data-path="${text(e.path)}" data-kind="${text(e.kind)}"
               aria-selected="false"
               ${e.escapes ? 'title="This link points outside the working directory and cannot be opened."' : ""}>
            <span class="check" aria-hidden="true"></span>
            <span class="kind" aria-hidden="true">${
              thumb ? `<img class="thumb" alt="" draggable="false" data-thumb="${text(e.path)}" />` : icon
            }</span>
            <span class="dj-row-name">${text(e.name)}</span>
            <span class="dj-row-meta">${text(metaOf(e))}</span>
          </div>`;
        })
        .join("");
    }
    drawState();
    thumbnails.watch();
  }

  /** Selected state, the bars and their buttons. No rows are rebuilt. */
  function drawState() {
    for (const row of list.querySelectorAll(".dj-row")) {
      const path = row.dataset.path;
      const selected = selection.has(path);
      row.setAttribute("aria-selected", String(selected));
      row.querySelector(".check").textContent = selected ? "✓" : "";
      if (selection.cursor === path) row.setAttribute("data-cursor", "");
      else row.removeAttribute("data-cursor");
    }
    list.dataset.mode = selection.mode ? "select" : "";
    const count = selection.selected.length;
    selbar.hidden = count === 0 && !selection.mode;
    selcount.textContent = count === 1 ? "1 selected" : `${count} selected`;
    const one = count === 1 ? byPath.get(selection.selected[0]) : null;
    const inTrash = view === "trash";
    $("rename").disabled = count !== 1;
    $("peek").disabled = !(one && one.kind !== "dir" && !one.escapes);
    for (const id of ["cut", "copy", "trash"]) $(id).disabled = count === 0;
    $("restore").disabled = count === 0;
    // The trash view has one action on a selection, and the folder view has
    // every action but that one.
    for (const id of ["peek", "rename", "cut", "copy", "trash"]) $(id).hidden = inTrash;
    $("restore").hidden = !inTrash;
    $("mkdir").hidden = inTrash;
    // Offered only where it can work: a desktop that opens app windows, and
    // one file that some app takes.
    const withable = one && !one.escapes && OPEN_WITH.some((o) => offered(o.app) && o.fits(one));
    $("openwith").hidden = inTrash || !OPEN_WITH.some((o) => offered(o.app));
    $("openwith").disabled = !withable;
    $("select").setAttribute("aria-pressed", String(selection.mode));
    pasteButton.hidden = !clip || inTrash;
    if (clip) {
      const n = clip.paths.length;
      pasteButton.textContent = `${clip.mode === "copy" ? "Copy" : "Move"} ${n} here`;
    }
    $("tools").hidden = inTrash;
  }

  function drawSpace() {
    if (!space || view === "trash") {
      spaceLine.textContent = "";
      spaceLine.removeAttribute("data-low");
      return;
    }
    const low =
      space.available < LOW_SPACE_BYTES || space.available < space.total * LOW_SPACE_FRACTION;
    spaceLine.textContent = low
      ? `Only ${size(space.available)} free`
      : `${size(space.available)} free`;
    spaceLine.title = `${size(space.available)} free of ${size(space.total)}`;
    if (low) spaceLine.setAttribute("data-low", "");
    else spaceLine.removeAttribute("data-low");
  }

  /** Free space is a nicety: if it cannot be read, the line is left empty. */
  async function refreshSpace() {
    try {
      space = await bus.invoke("files.space", {});
    } catch {
      space = null;
    }
    drawSpace();
  }

  function note() {
    const total = entries.length;
    const items = (n) => `${n} item${n === 1 ? "" : "s"}`;
    return filter ? `${shown.length} of ${items(total)}` : items(total);
  }

  // -- thumbnails -----------------------------------------------------------

  /**
   * Thumbnails for the image rows, asked for as rows come into view, a few at
   * a time, and remembered for the life of the frame by address, size and
   * time so a refresh does not ask again for what has not changed.
   */
  const thumbnails = (() => {
    const known = new Map(); // key -> data URL, or null when there is none
    const queue = [];
    let running = 0;
    let generation = 0;
    let observer = null;

    const keyOf = (e) => `${e.path}|${e.size ?? ""}|${e.modified ?? ""}`;

    function paint(img, url) {
      if (url) {
        img.src = url;
      }
    }

    function pump() {
      while (running < THUMBNAIL_CONCURRENCY && queue.length) {
        const { path, key, gen } = queue.shift();
        if (gen !== generation) continue;
        running++;
        bus
          .invoke("files.thumbnail", { path, size: THUMBNAIL_SIZE })
          .then(
            (t) => `data:${t.mime};base64,${t.content}`,
            () => null,
          )
          .then((url) => {
            running--;
            known.set(key, url);
            if (gen === generation) {
              const img = list.querySelector(`img[data-thumb="${CSS.escape(path)}"]`);
              if (img) paint(img, url);
            }
            pump();
          });
      }
    }

    function want(img) {
      const e = byPath.get(img.dataset.thumb);
      if (!e) return;
      const key = keyOf(e);
      if (known.has(key)) {
        paint(img, known.get(key));
        return;
      }
      if (queue.some((q) => q.key === key)) return;
      queue.push({ path: e.path, key, gen: generation });
      pump();
    }

    return {
      /** The rows were redrawn: forget what was queued for the old ones. */
      watch() {
        generation++;
        queue.length = 0;
        observer?.disconnect();
        observer = null;
        const imgs = [...list.querySelectorAll("img[data-thumb]")];
        if (imgs.length === 0) return;
        if (Observer) {
          // The row is what is watched, not the picture: an image with no
          // source yet takes no space, and an element that takes no space
          // never intersects anything, so watching it would wait forever.
          observer = new Observer(
            (seen) => {
              for (const s of seen) {
                if (!s.isIntersecting) continue;
                observer?.unobserve(s.target);
                const img = s.target.querySelector("img[data-thumb]");
                if (img) want(img);
              }
            },
            { root: list, rootMargin: "200px 0px" },
          );
          for (const img of imgs) observer.observe(img.closest(".dj-row"));
        } else {
          for (const img of imgs) want(img);
        }
      },
    };
  })();

  // -- navigation -----------------------------------------------------------

  function listed(result) {
    return (result.entries ?? []).map((e) => ({ ...e, path: local(e.path ?? child(e.name)) }));
  }

  async function go(path) {
    const result = await bus.invoke("files.list", { path });
    const next = local(result.path ?? path);
    const moved = view !== "folder" || next !== here;
    view = "folder";
    here = next;
    if (moved) {
      filter = "";
      filterInput.value = "";
    }
    drawCrumbs();
    entries = listed(result);
    byPath.clear();
    for (const e of entries) byPath.set(e.path, e);
    arrange();
    // A new folder starts with nothing selected and selection mode off: a
    // selection carried into another folder is one you cannot see.
    selection.setMode(false);
    selection.setItems(shown.map((e) => e.path));
    drawRows();
    let line = note();
    if (result.truncated) line += " (listing was cut short)";
    say(line);
    offerKeepBoth(null);
    refreshSpace();
  }

  /** The trash, as rows: keyed by trash id, named by where each came from. */
  async function goTrash() {
    const result = await bus.invoke("files.trash.list", {});
    view = "trash";
    filter = "";
    filterInput.value = "";
    drawCrumbs();
    entries = (result.entries ?? []).map((e) => ({
      path: e.id,
      name: local(e.from),
      kind: e.kind,
      size: e.len ?? e.size,
      meta: e.deleted_at ? e.deleted_at.replace("T", " ") : "",
    }));
    byPath.clear();
    for (const e of entries) byPath.set(e.path, e);
    arrange();
    selection.setMode(false);
    selection.setItems(shown.map((e) => e.path));
    drawRows();
    drawSpace();
    offerKeepBoth(null);
    say(
      entries.length === 0
        ? "The trash is empty."
        : `${entries.length} in the trash. To delete them for good, run "Empty the trash" from the palette.`,
    );
  }

  /** Re-list the same folder, keeping the selection that still applies. */
  async function refresh() {
    const keep = selection.selected;
    const mode = selection.mode;
    if (view === "trash") await goTrash();
    else await go(here);
    if (mode) selection.setMode(true);
    for (const path of keep) if (byPath.has(path)) selection.click(path, { toggle: true });
  }

  /** The files of this folder, in display order: what Quick Look steps through. */
  function previewable() {
    return shown.filter((e) => e.kind !== "dir" && !e.escapes).map((e) => e.path);
  }

  function peek(path) {
    const e = byPath.get(path);
    // A trash row's key is a trash id, not an address; there is nothing at
    // it for Quick Look to fetch.
    if (!e || view === "trash" || e.kind === "dir" || e.escapes) return;
    if (!shell.quicklook(path, previewable())) {
      say("Quick Look is part of the desktop; open Files from the dock to use it.");
    }
  }

  function open(path) {
    const e = byPath.get(path);
    if (!e || view === "trash") return;
    if (e.escapes) {
      say("This link points outside the working directory and cannot be opened.");
      return;
    }
    if (e.kind === "dir") go(path).catch((err) => failure(status, err));
    // A text file goes to the editor unless the desktop has said it has
    // none; if it refuses when asked, the file is shown in Quick Look.
    else if (isText(e.name) && opener() && installed.get(EDITOR) !== false) {
      openWith(EDITOR, path, { fallback: true });
    } else peek(path);
  }

  function up() {
    if (view === "trash") {
      go(here).catch((e) => failure(status, e));
      return;
    }
    if (here === "/") return;
    go(parentOf(here)).catch((e) => failure(status, e));
  }

  /** Inline rename: the row's name becomes a field. Enter commits. */
  function rename() {
    const [path] = selection.selected;
    const e = byPath.get(path);
    if (!e || selection.selected.length !== 1 || view === "trash") return;
    const row = list.querySelector(`.dj-row[data-path="${CSS.escape(path)}"]`);
    const name = row?.querySelector(".dj-row-name");
    if (!name) return;
    const input = doc.createElement("input");
    input.className = "rename";
    input.value = e.name;
    input.setAttribute("aria-label", `New name for ${e.name}`);
    name.replaceWith(input);
    input.focus();
    // Select the stem, not the extension: renaming a photo is about the name.
    const dot = e.kind === "dir" ? -1 : e.name.lastIndexOf(".");
    input.setSelectionRange(0, dot > 0 ? dot : e.name.length);

    let done = false;
    const finish = async (commit) => {
      if (done) return;
      done = true;
      const to = input.value.trim();
      if (!commit || to === "" || to === e.name) {
        drawRows();
        list.focus();
        return;
      }
      if (to.includes("/")) {
        say("A name cannot contain '/'. To move something, use Move.");
        drawRows();
        return;
      }
      try {
        await bus.invoke("files.rename", { from: path, to: child(to) });
        await go(here);
        selection.click(child(to));
        say(`Renamed to ${to}`);
      } catch (err) {
        drawRows();
        failure(status, err);
      }
      list.focus();
    };
    input.addEventListener("keydown", (ev) => {
      ev.stopPropagation();
      if (ev.key === "Enter") finish(true);
      else if (ev.key === "Escape") finish(false);
    });
    input.addEventListener("blur", () => finish(true));
  }

  // -- cut, copy, paste, drag, trash ----------------------------------------

  function pick(mode) {
    const paths = selection.selected;
    if (view === "trash" || paths.length === 0) return;
    // "move" or "copy", never "cut": Cut is the gesture, a move is what it
    // asks for, and everything downstream speaks in commands. The sizes are
    // kept so a copy can be checked against the free space before it starts;
    // a folder's size is not known here, so a clip with one is not checked.
    const sizes = paths.map((p) => {
      const e = byPath.get(p);
      return e && e.kind === "file" ? (e.size ?? 0) : null;
    });
    clip = {
      mode: mode === "copy" ? "copy" : "move",
      paths,
      bytes: sizes.includes(null) ? null : sizes.reduce((a, b) => a + b, 0),
    };
    const n = paths.length;
    say(`${n} item${n === 1 ? "" : "s"} ready to ${clip.mode}. Go to a folder and paste.`);
    drawState();
  }

  const plural = (n) => `${n} item${n === 1 ? "" : "s"}`;

  /**
   * Move or copy `paths` into `to`. The core checks every name before
   * anything moves and puts back what already moved if anything fails, so
   * the result is all or nothing and the message is the whole story.
   */
  async function transfer(mode, paths, to, { keepBoth = false } = {}) {
    const command = mode === "copy" ? "files.copy" : "files.move";
    const params = { paths, to };
    if (keepBoth) params.keep_both = true;
    say(mode === "copy" ? "Copying…" : "Moving…");
    offerKeepBoth(null);
    try {
      const result = await bus.invoke(command, params, (p) => {
        if (typeof p.fraction === "number") say(`Copying… ${Math.round(p.fraction * 100)}%`);
      });
      offerUndo(null);
      return (result.paths ?? []).map(local);
    } catch (e) {
      failure(status, e);
      // A copy refused for a taken name can be done under "name copy"
      // instead. A move is not offered that: a move that renamed would leave
      // the operator looking for a file under a name they never chose.
      if (mode === "copy" && /already exists/.test(String(e?.message ?? e))) {
        offerKeepBoth({ paths, to });
      }
      return null;
    }
  }

  async function paste() {
    if (!clip || view === "trash") return;
    const { mode, paths, bytes } = clip;
    const sameFolder = paths.every((p) => parentOf(p) === here);
    if (mode === "move" && sameFolder) {
      say(paths.length === 1 ? "It is already here." : "They are already here.");
      return;
    }
    if (mode === "copy" && bytes != null && space && bytes > space.available) {
      say(`Not enough room: this needs ${size(bytes)} and ${size(space.available)} is free.`);
      return;
    }
    // Pasting a copy where it came from asks for a second one, so it is
    // named "name copy" rather than refused as a name already taken.
    const landed = await transfer(mode, paths, here, { keepBoth: mode === "copy" && sameFolder });
    if (!landed) return;
    // A copy can be pasted again; a move cannot, its sources are gone.
    if (mode === "move") clip = null;
    await go(here);
    for (const p of landed) if (byPath.has(p)) selection.click(p, { toggle: true });
    say(`${mode === "copy" ? "Copied" : "Moved"} ${plural(paths.length)}.`);
  }

  async function keepBoth() {
    if (!retry) return;
    const { paths, to } = retry;
    const landed = await transfer("copy", paths, to, { keepBoth: true });
    if (!landed) return;
    await go(here);
    for (const p of landed) if (byPath.has(p)) selection.click(p, { toggle: true });
    say(`Copied ${plural(paths.length)}, keeping both.`);
  }

  /** A drop: move into a folder and stay where you are. */
  async function dropInto(paths, to) {
    const landed = await transfer("move", paths, to);
    if (!landed) return;
    if (clip) {
      clip.paths = clip.paths.filter((p) => !paths.includes(p));
      if (clip.paths.length === 0) clip = null;
    }
    await refresh();
    const where = to === "/" ? "the working directory" : nameOf(to);
    say(`Moved ${plural(paths.length)} to ${where}.`);
  }

  async function toTrash() {
    const paths = selection.selected;
    if (view === "trash" || paths.length === 0) return;
    try {
      const result = await bus.invoke("files.trash", { paths });
      const ids = (result.trashed ?? []).map((t) => t.id);
      // Anything on the clipboard that just went to the trash is gone from
      // where it was; pasting it would only fail.
      if (clip) {
        clip.paths = clip.paths.filter((p) => !paths.includes(p));
        if (clip.paths.length === 0) clip = null;
      }
      await refresh();
      const n = ids.length;
      say(`Moved ${n} item${n === 1 ? "" : "s"} to the trash.`);
      offerUndo(ids);
    } catch (e) {
      failure(status, e);
    }
  }

  async function restore(ids) {
    if (!ids || ids.length === 0) return;
    try {
      await bus.invoke("files.trash.restore", { ids });
      offerUndo(null);
      await refresh();
      say(`Put back ${ids.length} item${ids.length === 1 ? "" : "s"}.`);
    } catch (e) {
      failure(status, e);
    }
  }

  async function mkdir() {
    const name = win.prompt("New folder name");
    if (!name) return;
    try {
      await bus.invoke("files.mkdir", { path: child(name) });
      await refresh();
    } catch (e) {
      failure(status, e);
    }
  }

  // -- the menu -------------------------------------------------------------

  /** What the menu offers for the current selection, in order. */
  function menuItems() {
    const paths = selection.selected;
    const one = paths.length === 1 ? byPath.get(paths[0]) : null;
    if (view === "trash") {
      return paths.length ? [{ label: "Restore", run: () => restore(paths) }] : [];
    }
    const items = [];
    if (one && !one.escapes) {
      items.push({ label: "Open", run: () => open(one.path) });
      for (const o of OPEN_WITH) {
        if (offered(o.app) && o.fits(one)) {
          items.push({ label: `Open with ${o.label}`, run: () => openWith(o.app, one.path) });
        }
      }
      if (one.kind !== "dir") items.push({ label: "Quick Look", run: () => peek(one.path) });
      items.push({ label: "Rename", run: rename });
    }
    if (paths.length) {
      items.push({ label: "Cut", run: () => pick("cut") });
      items.push({ label: "Copy", run: () => pick("copy") });
      items.push({ label: "Move to the trash", run: toTrash });
    }
    if (clip) items.push({ label: pasteButton.textContent || "Paste", run: paste });
    return items;
  }

  let menuActions = [];

  function closeMenu() {
    menu.hidden = true;
    menuActions = [];
  }

  /** Show `items` at a point in the frame, kept inside it. */
  function showMenu(items, x, y) {
    if (items.length === 0) return;
    menuActions = items;
    menu.innerHTML = items
      .map(
        (it, i) =>
          `<button class="menu-item" role="menuitem" data-item="${i}">${text(it.label)}</button>`,
      )
      .join("");
    menu.hidden = false;
    const w = win.innerWidth || 0;
    const h = win.innerHeight || 0;
    const r = menu.getBoundingClientRect?.() ?? { width: 0, height: 0 };
    menu.style.left = `${Math.max(4, Math.min(x, w - r.width - 4))}px`;
    menu.style.top = `${Math.max(4, Math.min(y, h - r.height - 4))}px`;
    menu.querySelector(".menu-item")?.focus();
  }

  function openWithMenu() {
    const [path] = selection.selected;
    const one = byPath.get(path);
    if (!one || !opener()) return;
    const items = OPEN_WITH.filter((o) => offered(o.app) && o.fits(one)).map((o) => ({
      label: o.label,
      run: () => openWith(o.app, one.path),
    }));
    if (one.kind !== "dir") items.push({ label: "Quick Look", run: () => peek(one.path) });
    const b = $("openwith").getBoundingClientRect?.() ?? { left: 0, bottom: 0 };
    showMenu(items, b.left, b.bottom);
  }

  menu.addEventListener("click", (ev) => {
    const item = ev.target.closest?.(".menu-item");
    if (!item) return;
    const action = menuActions[Number(item.dataset.item)];
    closeMenu();
    action?.run();
  });
  menu.addEventListener("keydown", (ev) => {
    ev.stopPropagation();
    const items = [...menu.querySelectorAll(".menu-item")];
    const at = items.indexOf(doc.activeElement);
    if (ev.key === "Escape") {
      closeMenu();
      list.focus();
    } else if (ev.key === "ArrowDown") items[(at + 1) % items.length]?.focus();
    else if (ev.key === "ArrowUp") items[(at - 1 + items.length) % items.length]?.focus();
    else return;
    ev.preventDefault();
  });
  doc.addEventListener("pointerdown", (ev) => {
    if (!menu.hidden && !menu.contains(ev.target)) closeMenu();
  });

  // -- pointer and touch ----------------------------------------------------

  const rowOf = (target) => target?.closest?.(".dj-row");

  /** A long-press in progress: where it started, and how to cancel it. */
  let press = null;
  /** A pointer down on a row that may become a drag. */
  let pending = null;
  /** A drag in progress. */
  let drag = null;
  /** Set when a drag ends, so the click that follows it is not a click. */
  let swallowClick = false;

  /** Where a drop at this point would go, or null. */
  function dropTarget(x, y) {
    const el = hitTest(x, y);
    const row = el?.closest?.('.dj-row[data-kind="dir"]');
    const crumb = el?.closest?.(".crumb[data-path]");
    const path = row?.dataset.path ?? (crumb ? crumb.dataset.path : null);
    if (path == null || !drag) return null;
    if (path === here && crumb) return null;
    // Not onto itself, not into anything inside itself, not where it is.
    const inside = (p) => path === p || path.startsWith(`${p}/`);
    if (drag.paths.some(inside)) return null;
    if (drag.paths.every((p) => parentOf(p) === path)) return null;
    return { el: row ?? crumb, path };
  }

  function startDrag(path, x, y) {
    if (view === "trash") return;
    const paths = selection.has(path) ? selection.selected : [path];
    drag = { paths, over: null };
    ghost.textContent = `Move ${plural(paths.length)}`;
    ghost.hidden = false;
    list.setAttribute("data-dragging", "");
    moveDrag(x, y);
  }

  function moveDrag(x, y) {
    ghost.style.transform = `translate(${x + 12}px, ${y + 12}px)`;
    const target = dropTarget(x, y);
    if (drag.over?.el !== target?.el) {
      drag.over?.el.removeAttribute("data-drop");
      target?.el.setAttribute("data-drop", "");
    }
    drag.over = target;
    steer(x, y);
  }

  // -- scrolling while dragging ---------------------------------------------
  //
  // A finger held near the top or bottom of the list scrolls it, faster the
  // nearer the edge. Without this a touch drag could only reach folder rows
  // already on screen: once the long-press fires, the finger belongs to the
  // drag and the list cannot be scrolled by hand (see `touchmove` below),
  // and folders sort first, so the one you want is often scrolled away.

  /** The band at each edge that scrolls, and the fastest step, in pixels. */
  const EDGE = 48;
  const EDGE_STEP = 24;
  const EDGE_TICK_MS = 16;
  /** Where the pointer was last seen during a drag. */
  let held = null;
  /** Cancels the next scroll step, while one is scheduled. */
  let scrolling = null;

  function edgeSpeed(y) {
    const r = list.getBoundingClientRect?.();
    if (!r || !r.height) return 0;
    const band = Math.min(EDGE, r.height / 4);
    if (y < r.top + band) return -Math.ceil((EDGE_STEP * (r.top + band - y)) / band);
    if (y > r.bottom - band) return Math.ceil((EDGE_STEP * (y - (r.bottom - band))) / band);
    return 0;
  }

  function steer(x, y) {
    held = { x, y };
    if (!scrolling && edgeSpeed(y)) scrolling = delay(EDGE_TICK_MS, scrollStep);
  }

  function scrollStep() {
    scrolling = null;
    if (!drag || !held) return;
    const speed = edgeSpeed(held.y);
    if (!speed) return;
    const before = list.scrollTop;
    list.scrollTop = before + speed;
    // At the end of the list there is nothing more to do until it moves.
    if (list.scrollTop === before) return;
    // The rows moved under a pointer that did not: what is under it now?
    moveDrag(held.x, held.y);
  }

  function stopScrolling() {
    scrolling?.();
    scrolling = null;
    held = null;
  }

  function endDrag(commit) {
    if (!drag) return;
    stopScrolling();
    const { paths, over } = drag;
    over?.el.removeAttribute("data-drop");
    ghost.hidden = true;
    list.removeAttribute("data-dragging");
    drag = null;
    swallowClick = true;
    if (commit && over) dropInto(paths, over.path);
  }

  function stopTracking() {
    pending = null;
    doc.removeEventListener("pointermove", onMove);
    doc.removeEventListener("pointerup", onUp);
    doc.removeEventListener("pointercancel", onCancel);
  }

  function onMove(ev) {
    if (drag) {
      moveDrag(ev.clientX, ev.clientY);
      return;
    }
    if (!pending) return;
    const far = Math.hypot(ev.clientX - pending.x, ev.clientY - pending.y);
    if (pending.touch) {
      if (press?.fired) {
        // Held, then moved: a drag.
        if (far > LONG_PRESS_SLOP) startDrag(pending.path, ev.clientX, ev.clientY);
      } else if (far > LONG_PRESS_SLOP) {
        // Moved before the hold: a scroll, which the browser owns.
        press?.cancel();
        press = null;
        stopTracking();
      }
    } else if (far > DRAG_SLOP) {
      startDrag(pending.path, ev.clientX, ev.clientY);
    }
  }

  function onUp() {
    if (drag) endDrag(true);
    stopTracking();
  }

  function onCancel() {
    endDrag(false);
    press?.cancel();
    press = null;
    stopTracking();
  }

  list.addEventListener("pointerdown", (ev) => {
    swallowClick = false;
    // A mouse or pen pressed means any touch long-press is over, whether or
    // not a click came to end it (Chrome's long-press gesture sends none).
    // Left standing, it would swallow this click and every right-click.
    if (ev.pointerType !== "touch" && press) {
      press.cancel();
      press = null;
    }
    const row = rowOf(ev.target);
    if (!row || list.querySelector("input.rename")) return;
    const touch = ev.pointerType === "touch";
    // The primary button only: a right-click is the menu, not a drag.
    if (!touch && ev.button != null && ev.button !== 0) return;
    const path = row.dataset.path;
    pending = { path, x: ev.clientX, y: ev.clientY, touch };
    doc.addEventListener("pointermove", onMove);
    doc.addEventListener("pointerup", onUp);
    doc.addEventListener("pointercancel", onCancel);
    if (!touch) return;
    press = {
      fired: false,
      cancel: delay(LONG_PRESS_MS, () => {
        press.fired = true;
        if (!selection.mode) selection.setMode(true);
        if (!selection.has(path)) selection.click(path, { toggle: true });
      }),
    };
  });

  // A drag here is Files' own, on pointer events. The browser's native drag
  // of a thumbnail (or of selected text) would start on the same gesture and
  // cancel the pointer Files is tracking, so it never starts.
  list.addEventListener("dragstart", (ev) => ev.preventDefault());

  // Once a long-press has fired the finger belongs to Files, not to the
  // scroller: without this the browser starts scrolling on the first move
  // and cancels the pointer, and a drag on a phone could never begin.
  list.addEventListener(
    "touchmove",
    (ev) => {
      if (press?.fired || drag) ev.preventDefault();
    },
    { passive: false },
  );

  list.addEventListener("click", (ev) => {
    if (swallowClick) {
      swallowClick = false;
      press = null;
      return;
    }
    const row = rowOf(ev.target);
    if (!row) return;
    const path = row.dataset.path;
    const touch = ev.pointerType === "touch" || press !== null;
    if (press) {
      press.cancel();
      const fired = press.fired;
      press = null;
      // The long-press already did its thing; the click that ends it must
      // not toggle the row straight back.
      if (fired) return;
    }
    if (touch && !selection.mode) {
      open(path);
      return;
    }
    selection.click(path, { toggle: ev.ctrlKey || ev.metaKey, extend: ev.shiftKey });
  });

  list.addEventListener("dblclick", (ev) => {
    const row = rowOf(ev.target);
    if (row && !selection.mode) open(row.dataset.path);
  });

  list.addEventListener("contextmenu", (ev) => {
    const row = rowOf(ev.target);
    if (!row) return;
    ev.preventDefault();
    // A long-press on a phone also fires this; the long-press has its own
    // meaning there (selection mode, then perhaps a drag).
    if (press) return;
    if (!selection.has(row.dataset.path)) selection.click(row.dataset.path);
    showMenu(menuItems(), ev.clientX, ev.clientY);
  });

  crumbs.addEventListener("click", (ev) => {
    const button = ev.target.closest?.(".crumb[data-path]");
    if (button) go(button.dataset.path).catch((e) => failure(status, e));
  });

  // -- keyboard -------------------------------------------------------------

  list.addEventListener("keydown", (ev) => {
    const mod = ev.ctrlKey || ev.metaKey;
    const cursor = selection.cursor;
    if (drag && ev.key === "Escape") {
      endDrag(false);
      stopTracking();
      ev.preventDefault();
      return;
    }
    switch (ev.key) {
      case "ArrowDown":
        selection.move(1, { extend: ev.shiftKey });
        break;
      case "ArrowUp":
        if (ev.altKey) up();
        else selection.move(-1, { extend: ev.shiftKey });
        break;
      case "Home":
        selection.move(-shown.length, { extend: ev.shiftKey });
        break;
      case "End":
        selection.move(shown.length, { extend: ev.shiftKey });
        break;
      case "Enter":
        if (cursor) open(cursor);
        break;
      case " ":
        if (cursor) peek(cursor);
        break;
      case "Backspace":
        up();
        break;
      case "F2":
        rename();
        break;
      case "F10":
        if (!ev.shiftKey) return;
        openMenuAtCursor();
        break;
      case "ContextMenu":
        openMenuAtCursor();
        break;
      case "/":
        filterInput.focus();
        break;
      case "Escape":
        if (selection.mode) selection.setMode(false);
        else selection.clear();
        break;
      case "Delete":
        toTrash();
        break;
      case "x":
      case "X":
        if (!mod) return;
        pick("cut");
        break;
      case "c":
      case "C":
        if (!mod) return;
        pick("copy");
        break;
      case "v":
      case "V":
        if (!mod) return;
        paste();
        break;
      case "a":
      case "A":
        if (!mod) return;
        selection.selectAll();
        break;
      default:
        return;
    }
    ev.preventDefault();
  });

  function openMenuAtCursor() {
    const cursor = selection.cursor;
    if (cursor && !selection.has(cursor)) selection.click(cursor);
    const row = cursor && list.querySelector(`.dj-row[data-path="${CSS.escape(cursor)}"]`);
    const r = row?.getBoundingClientRect?.() ?? { left: 0, bottom: 0 };
    showMenu(menuItems(), r.left + 16, r.bottom);
  }

  // -- the bars -------------------------------------------------------------

  filterInput.addEventListener("input", () => {
    filter = filterInput.value;
    arrange();
    selection.setItems(shown.map((e) => e.path));
    drawRows();
    say(note());
  });
  filterInput.addEventListener("keydown", (ev) => {
    if (ev.key === "Escape") {
      filterInput.value = "";
      filterInput.dispatchEvent(new win.Event("input"));
      list.focus();
    } else if (ev.key === "Enter" || ev.key === "ArrowDown") {
      list.focus();
      if (!selection.cursor && shown.length) selection.move(1);
    } else return;
    ev.preventDefault();
  });
  sortSelect.addEventListener("change", () => {
    if (!SORTS.includes(sortSelect.value)) return;
    // Size and time read best largest and newest first; names A to Z.
    sort = { key: sortSelect.value, dir: sortSelect.value === "name" || sortSelect.value === "kind" ? 1 : -1 };
    saveSort();
    resort();
  });
  sortDir.addEventListener("click", () => {
    sort = { ...sort, dir: -sort.dir };
    saveSort();
    resort();
  });

  function resort() {
    drawSortControls();
    arrange();
    selection.setItems(shown.map((e) => e.path));
    drawRows();
  }

  $("up").addEventListener("click", up);
  $("mkdir").addEventListener("click", mkdir);
  $("select").addEventListener("click", () => selection.setMode(!selection.mode));
  $("peek").addEventListener("click", () => peek(selection.selected[0]));
  $("openwith").addEventListener("click", openWithMenu);
  $("rename").addEventListener("click", rename);
  $("done").addEventListener("click", () => selection.setMode(false));
  $("cut").addEventListener("click", () => pick("cut"));
  $("copy").addEventListener("click", () => pick("copy"));
  $("trash").addEventListener("click", toTrash);
  $("restore").addEventListener("click", () => restore(selection.selected));
  pasteButton.addEventListener("click", paste);
  undoButton.addEventListener("click", () => restore(undo));
  keepBothButton.addEventListener("click", keepBoth);
  $("trashview").addEventListener("click", () => goTrash().catch((e) => failure(status, e)));

  // What the desktop can open, asked now and again whenever Files is
  // returned to: an app installed or removed meanwhile changes the menus.
  checkOpeners();
  win.addEventListener?.("focus", checkOpeners);

  drawSortControls();
  const ready = go("/").catch((e) => failure(list, e));
  return {
    ready,
    selection,
    go,
    refresh,
    get here() {
      return here;
    },
    get view() {
      return view;
    },
  };
}
