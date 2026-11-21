/**
 * The Editor app's UI: CodeMirror 6 over the core's `editor.*` commands.
 *
 * `mountEditor(document, deps)` builds the whole thing against whatever bus,
 * CodeMirror and storage it is handed, so the tests mount the real markup
 * with the real (vendored) CodeMirror and a fake bus, and production mounts
 * it with `/kit/bus.js`. Nothing here reads a global it was not given.
 *
 * What it promises, and where each promise is kept:
 *
 * - **Several files at once.** One `EditorView`, one `EditorState` per
 *   buffer, swapped on a tab switch. The state carries its own undo history,
 *   so undo is per file and survives a save (a save never replaces the
 *   state) and a revert (which is one ordinary, undoable change).
 * - **Nothing is lost to someone else's write.** Every buffer remembers the
 *   content `version` it was opened at and saves against it; the core answers
 *   `conflict` rather than clobbering, and the operator chooses. While a file
 *   is open its `modified` and size are polled with `files.stat`: a clean
 *   buffer reloads itself, a dirty one is told.
 * - **Nothing is lost to a reload.** Unsaved text is kept in this device's
 *   storage with the version it was based on, and restored on the next load;
 *   the next save then conflicts if the file moved on meanwhile.
 * - **Big files are honest.** The core decides (`editor.open`): editable,
 *   whole and read-only, or the tail of the file read-only with "Load
 *   earlier". The UI shows the reason it was given.
 * - **A phone can do all of it.** Save, Undo, Redo, Find and replace, Go to
 *   line and Wrap are buttons; the search panel opens at the top, where the
 *   keyboard cannot cover it; and the view is padded by however much of the
 *   frame the on-screen keyboard covers, so the cursor stays visible.
 */

/** Where this device keeps the editor's session (open files, unsaved text)
 * when it is not in a shell window. In one, it is the window's own key,
 * which the shell removes when the window closes. */
export const SESSION_KEY = "deckjack.editor.session";

/** The shell's per-window storage key; `windowStorageKey` in the shell's
 * windows store, which removes it when that window closes. */
export function windowSessionKey(windowId) {
  return `deckjack.window.${windowId}`;
}
export const WRAP_KEY = "deckjack.editor.wrap";

/** How often an open file is checked for a change on disk. */
export const POLL_MS = 4000;

/** Unsaved text larger than this is not kept across a reload: storage is a
 * few megabytes per origin and shared with every app on it. */
const DRAFT_MAX = 512 * 1024;

/** How much of a huge file "Load earlier" will accumulate. */
export const WINDOW_TOTAL_MAX = 4 * 1024 * 1024;

/** The message the shell posts to an open editor to open another file. */
export const OPEN_MESSAGE = "deckjack.app.open";

/**
 * The language for a file name, by the CodeMirror module's constructors.
 * `null` is plain text. Exported for its test; a wrong guess here is a file
 * highlighted as something it is not.
 */
export function languageFor(name, cm) {
  const lower = String(name ?? "").toLowerCase();
  const base = lower.slice(lower.lastIndexOf("/") + 1);
  const ext = base.includes(".") ? base.slice(base.lastIndexOf(".") + 1) : "";
  const stream = (mode) => cm.StreamLanguage.define(mode);
  const byName = {
    dockerfile: () => stream(cm.dockerFile),
    makefile: () => stream(cm.shell),
    "cargo.lock": () => stream(cm.toml),
    ".bashrc": () => stream(cm.shell),
    ".zshrc": () => stream(cm.shell),
    ".profile": () => stream(cm.shell),
    ".gitignore": () => stream(cm.properties),
  };
  if (byName[base]) return byName[base]();
  const byExt = {
    js: () => cm.javascript(),
    mjs: () => cm.javascript(),
    cjs: () => cm.javascript(),
    jsx: () => cm.javascript({ jsx: true }),
    ts: () => cm.javascript({ typescript: true }),
    mts: () => cm.javascript({ typescript: true }),
    tsx: () => cm.javascript({ typescript: true, jsx: true }),
    vue: () => cm.html(),
    rs: () => cm.rust(),
    py: () => cm.python(),
    json: () => cm.json(),
    webmanifest: () => cm.json(),
    md: () => cm.markdown(),
    markdown: () => cm.markdown(),
    html: () => cm.html(),
    htm: () => cm.html(),
    css: () => cm.css(),
    yaml: () => cm.yaml(),
    yml: () => cm.yaml(),
    c: () => cm.cpp(),
    h: () => cm.cpp(),
    cc: () => cm.cpp(),
    cpp: () => cm.cpp(),
    hpp: () => cm.cpp(),
    go: () => cm.go(),
    java: () => cm.java(),
    sql: () => cm.sql(),
    xml: () => cm.xml(),
    svg: () => cm.xml(),
    toml: () => stream(cm.toml),
    sh: () => stream(cm.shell),
    bash: () => stream(cm.shell),
    zsh: () => stream(cm.shell),
    lua: () => stream(cm.lua),
    diff: () => stream(cm.diff),
    patch: () => stream(cm.diff),
    ini: () => stream(cm.properties),
    conf: () => stream(cm.properties),
    properties: () => stream(cm.properties),
    env: () => stream(cm.properties),
    nginx: () => stream(cm.nginx),
  };
  return byExt[ext]?.() ?? null;
}

/** A short human name for the language, for the status line. */
export function languageLabel(name) {
  const lower = String(name ?? "").toLowerCase();
  const ext = lower.includes(".") ? lower.slice(lower.lastIndexOf(".") + 1) : "";
  const labels = {
    rs: "Rust", js: "JavaScript", mjs: "JavaScript", ts: "TypeScript", tsx: "TSX", jsx: "JSX",
    py: "Python", json: "JSON", md: "Markdown", html: "HTML", css: "CSS", yaml: "YAML",
    yml: "YAML", toml: "TOML", sh: "Shell", lua: "Lua", go: "Go", java: "Java", sql: "SQL",
    c: "C", h: "C", cpp: "C++", xml: "XML", diff: "Diff", vue: "Vue",
  };
  return labels[ext] ?? "Plain text";
}

/**
 * How much of the frame the on-screen keyboard covers, in CSS pixels.
 *
 * `frameBottom` is the bottom of this document's frame in the top window's
 * layout coordinates, and the visual viewport is the part of the top window
 * the person can see. Whatever of the frame is below the visible part is
 * under the keyboard (or off screen, which is the same problem).
 */
export function keyboardInset({ frameBottom, viewportTop, viewportHeight }) {
  if (![frameBottom, viewportTop, viewportHeight].every(Number.isFinite)) return 0;
  return Math.max(0, Math.round(frameBottom - (viewportTop + viewportHeight)));
}

/**
 * A path from the shell or the operator, made into a file address.
 *
 * Not trimmed: the core does not trim names, so `/notes.md ` is a different
 * file from `/notes.md`, and a path from Files has to open (and save into)
 * the file it names. Text the operator typed is trimmed where it is read.
 */
export function normalizePath(path) {
  if (typeof path !== "string") return null;
  if (path.trim() === "" || path.includes("\0")) return null;
  // `node:/path` and `/path` are addresses already; a bare relative path is
  // relative to the working directory, which is the root.
  if (/^[^/]*:\//.test(path) || path.startsWith("/")) return path;
  return `/${path}`;
}

/** The address without its node, for display and for matching. */
function local(path) {
  return String(path ?? "").replace(/^[^/:]+:(?=\/)/, "");
}

function baseName(path) {
  const p = local(path);
  return p.slice(p.lastIndexOf("/") + 1) || p;
}

function sizeText(bytes) {
  if (bytes >= 1024 * 1024) return `${(bytes / (1024 * 1024)).toFixed(1)} MB`;
  return `${Math.max(1, Math.ceil(bytes / 1024))} KB`;
}

/** The frame a save will travel in, measured the way the socket measures it. */
export function frameBytes(params) {
  const frame = JSON.stringify({ type: "invoke", id: "0000000000", command: "editor.save", params });
  return new TextEncoder().encode(frame).length;
}

/**
 * Mount the editor into `doc`, which already holds `index.html`'s markup.
 *
 * deps:
 * - `bus`: `{ invoke(command, params) }`, `/kit/bus.js` in production.
 * - `cm`: the vendored CodeMirror module.
 * - `storage`: `{ read(key), write(key, value) }`; never throws. `write`
 *   returns `false` when the value could not be kept (storage full or
 *   forbidden).
 * - `ask(question, value)`: a prompt; resolves to a string or null.
 * - `confirm(question)`: resolves to a boolean.
 * - `every(ms, fn)`: a repeating timer; returns a cancel function.
 * - `later(ms, fn)`: a one-shot timer; returns a cancel function.
 * - `win`: the window, for messages from the shell and the keyboard inset.
 *   Optional; the tests leave it out where it is not the subject.
 * - `shell`: `/kit/shell.js`'s `shell`, for Quick Look on a file that is not
 *   text. Optional.
 * - `windowId`: the shell's id for this window, which scopes the session kept
 *   on this device. Optional.
 * - `now()`: milliseconds, for the session's timestamp. Optional.
 */
export function mountEditor(doc, deps) {
  const { bus, cm, storage, ask, confirm, every, later } = deps;
  const win = deps.win ?? null;
  const now = deps.now ?? (() => Date.now());
  // One session per editor window, so two windows on one device do not
  // overwrite each other's unsaved text, kept where the shell clears it when
  // the window closes: unsaved text can be a secret.
  const sessionKey = deps.windowId ? windowSessionKey(deps.windowId) : SESSION_KEY;
  const $ = (id) => doc.getElementById(id);
  const el = {
    tabs: $("tabs"),
    host: $("host"),
    status: $("status"),
    position: $("position"),
    notice: $("notice"),
    noticeText: $("notice-text"),
    noticeActions: $("notice-actions"),
    save: $("save"),
    undo: $("undo"),
    redo: $("redo"),
    find: $("find"),
    gotoLine: $("goto"),
    wrap: $("wrap"),
    revert: $("revert"),
    open: $("open"),
    create: $("new"),
    earlier: $("earlier"),
    picker: $("picker"),
    pickerList: $("picker-list"),
    pickerPath: $("picker-path"),
    pickerGo: $("picker-go"),
    pickerClose: $("picker-close"),
    empty: $("empty"),
  };

  /** @type {Array<Buffer>} */
  const buffers = [];
  let active = null;
  let nextId = 1;
  let wrapOn = storage.read(WRAP_KEY) !== "off";
  let pickerDir = "/";
  const langs = new cm.Compartment();
  const wrapping = new cm.Compartment();
  const editable = new cm.Compartment();

  // -- presentation -------------------------------------------------------

  function say(message, kind = "info") {
    el.status.textContent = message;
    el.status.dataset.kind = kind;
  }

  function fail(error) {
    say(error instanceof Error ? error.message : String(error), "error");
  }

  const theme = cm.EditorView.theme({
    "&": { height: "100%", backgroundColor: "var(--dj-color-surface, #16181d)" },
    ".cm-scroller": {
      fontFamily: "var(--dj-font-mono, ui-monospace, monospace)",
      lineHeight: "1.55",
    },
    ".cm-content": { caretColor: "var(--dj-color-accent, #7aa2f7)" },
    ".cm-cursor, .cm-dropCursor": { borderLeftColor: "var(--dj-color-accent, #7aa2f7)" },
    ".cm-gutters": {
      backgroundColor: "var(--dj-color-raised, #1b1e25)",
      color: "var(--dj-color-text-muted, #9aa0ad)",
      borderRight: "1px solid var(--dj-color-border, #2a2e38)",
    },
    // A gutter wide enough to tap: a line number is how a line is selected
    // on a phone, where there is no Shift+Down.
    ".cm-lineNumbers .cm-gutterElement": { minWidth: "2.75rem", padding: "0 0.5rem 0 0.75rem" },
    ".cm-activeLine": { backgroundColor: "var(--dj-color-hairline, #21242c)" },
    ".cm-activeLineGutter": { backgroundColor: "var(--dj-color-hairline, #21242c)" },
    "&.cm-focused .cm-selectionBackground, .cm-selectionBackground, ::selection": {
      backgroundColor: "var(--dj-color-selection, #2d3344)",
    },
    ".cm-panels": {
      backgroundColor: "var(--dj-color-raised, #1b1e25)",
      color: "inherit",
    },
    ".cm-searchMatch": { backgroundColor: "var(--dj-color-warn-soft, #4a3d1a)" },
  });

  /** Tapping a line number selects the line. */
  const gutterSelect = cm.lineNumbers({
    domEventHandlers: {
      mousedown(view, line) {
        view.dispatch({ selection: { anchor: line.from, head: Math.min(line.to + 1, view.state.doc.length) } });
        return true;
      },
    },
  });

  function extensions(buffer) {
    const language = languageFor(buffer.name, cm);
    return [
      gutterSelect,
      cm.highlightActiveLineGutter(),
      cm.highlightSpecialChars(),
      cm.history(),
      cm.foldGutter(),
      cm.drawSelection(),
      cm.dropCursor(),
      cm.EditorState.allowMultipleSelections.of(true),
      cm.indentOnInput(),
      cm.syntaxHighlighting(cm.classHighlighter),
      cm.bracketMatching(),
      cm.closeBrackets(),
      cm.rectangularSelection(),
      cm.crosshairCursor(),
      cm.highlightActiveLine(),
      cm.highlightSelectionMatches(),
      // At the top, so the on-screen keyboard cannot cover the find field.
      cm.search({ top: true }),
      cm.keymap.of([
        ...cm.closeBracketsKeymap,
        ...cm.defaultKeymap,
        ...cm.searchKeymap,
        ...cm.historyKeymap,
        ...cm.foldKeymap,
        cm.indentWithTab,
      ]),
      theme,
      langs.of(language ?? []),
      wrapping.of(wrapOn ? cm.EditorView.lineWrapping : []),
      editable.of(
        buffer.mode === "edit"
          ? []
          : [cm.EditorState.readOnly.of(true), cm.EditorView.editable.of(false)],
      ),
      cm.EditorView.updateListener.of(changed),
    ];
  }

  const view = new cm.EditorView({
    parent: el.host,
    state: cm.EditorState.create({ doc: "" }),
  });

  /**
   * Called by CodeMirror on every update of the active buffer. Keeping
   * `active.state` current on every update, not only edits, is what lets a
   * tab switch keep folds, the open search panel and the cursor as well as
   * the text.
   */
  function changed(update) {
    if (!active) return;
    active.state = update.state;
    if (update.docChanged) {
      remember();
      drawTabs();
    }
    if (update.docChanged || update.selectionSet) drawState();
  }

  function isDirty(buffer) {
    if (buffer.mode !== "edit") return false;
    const state = buffer === active ? view.state : buffer.state;
    return !state.doc.eq(buffer.saved);
  }

  function drawTabs() {
    el.tabs.replaceChildren(
      ...buffers.map((b) => {
        const tab = doc.createElement("div");
        tab.className = "tab";
        tab.setAttribute("role", "tab");
        tab.setAttribute("aria-selected", b === active ? "true" : "false");
        tab.dataset.path = b.path ?? "";
        tab.title = b.path ? local(b.path) : "Not saved yet";
        const pick = doc.createElement("button");
        pick.className = "tab-name";
        pick.textContent = `${isDirty(b) ? "● " : ""}${b.name}`;
        pick.addEventListener("click", () => activate(b));
        const close = doc.createElement("button");
        close.className = "tab-close";
        close.setAttribute("aria-label", `Close ${b.name}`);
        close.textContent = "×";
        close.addEventListener("click", () => void closeBuffer(b));
        tab.append(pick, close);
        return tab;
      }),
    );
    el.tabs.querySelector('[aria-selected="true"]')?.scrollIntoView?.({ inline: "nearest" });
  }

  function drawState() {
    const b = active;
    const editing = Boolean(b) && b.mode === "edit";
    el.save.disabled = !editing || !isDirty(b);
    el.undo.disabled = !editing || cm.undoDepth(view.state) === 0;
    el.redo.disabled = !editing || cm.redoDepth(view.state) === 0;
    el.revert.disabled = !b || !b.path;
    el.find.disabled = !b;
    el.gotoLine.disabled = !b;
    el.earlier.hidden = !(b && b.window && b.window.start > 0);
    el.wrap.setAttribute("aria-pressed", wrapOn ? "true" : "false");
    el.empty.hidden = Boolean(b);
    el.host.hidden = !b;
    if (!b) {
      el.position.textContent = "";
      return;
    }
    const head = view.state.selection.main.head;
    const line = view.state.doc.lineAt(head);
    const where = `Ln ${line.number}, Col ${head - line.from + 1}`;
    const what = b.mode === "edit" ? languageLabel(b.name) : `${languageLabel(b.name)} · read-only`;
    el.position.textContent = `${where} · ${what}`;
  }

  /** The bar above the text: one question at a time, with its buttons. */
  function notify(text, actions = []) {
    el.notice.hidden = false;
    el.noticeText.textContent = text;
    el.noticeActions.replaceChildren(
      ...actions.map(([label, fn]) => {
        const button = doc.createElement("button");
        button.className = "dj-button";
        button.textContent = label;
        button.addEventListener("click", () => {
          clearNotice();
          void Promise.resolve(fn()).catch(fail);
        });
        return button;
      }),
    );
  }

  function clearNotice() {
    el.notice.hidden = true;
    el.noticeText.textContent = "";
    el.noticeActions.replaceChildren();
  }

  /** What the active buffer has to tell the operator, if anything. */
  function drawNotice() {
    const b = active;
    if (!b) return clearNotice();
    if (b.conflict) return notifyConflict(b);
    if (b.external && isDirty(b)) return notifyExternal(b);
    if (b.reason) return notify(b.reason);
    clearNotice();
  }

  // -- buffers --------------------------------------------------------------

  /**
   * @typedef {object} Buffer
   * @property {number} id
   * @property {string|null} path  qualified address, or null until saved
   * @property {string} name
   * @property {any} state         EditorState
   * @property {any} saved         the Text last known to be on disk
   * @property {string|null} version
   * @property {number|null} modified
   * @property {number|null} size
   * @property {"edit"|"read_only"} mode
   * @property {string|null} reason
   * @property {{start:number,end:number}|null} window
   * @property {object|null} conflict
   * @property {object|null} external
   */

  function makeBuffer(opened) {
    const buffer = {
      id: nextId++,
      path: opened.path ?? null,
      name: opened.name ?? (opened.path ? baseName(opened.path) : "untitled"),
      state: null,
      saved: null,
      version: opened.version ?? null,
      modified: opened.modified ?? null,
      size: opened.size ?? null,
      mode: opened.mode ?? "edit",
      reason: opened.reason ?? null,
      window: opened.window ?? null,
      saveLimit: opened.save_limit ?? Infinity,
      conflict: null,
      external: null,
    };
    buffer.state = cm.EditorState.create({ doc: opened.text ?? "", extensions: extensions(buffer) });
    buffer.saved = buffer.state.doc;
    return buffer;
  }

  function activate(buffer) {
    active = buffer;
    view.setState(buffer.state);
    drawTabs();
    drawNotice();
    drawState();
    rememberSoon();
  }

  function find(path) {
    const want = local(path);
    return buffers.find((b) => b.path && local(b.path) === want) ?? null;
  }

  /** Open a file, or switch to it if it is already open. */
  async function open(path) {
    const address = normalizePath(path);
    if (!address) {
      say("That is not a file address.", "error");
      return null;
    }
    const existing = find(address);
    if (existing) {
      activate(existing);
      return existing;
    }
    say(`Opening ${local(address)}…`);
    let opened;
    try {
      opened = await bus.invoke("editor.open", { path: address });
    } catch (error) {
      fail(error);
      // A file that is not text has somewhere better to go.
      if (/not text/.test(String(error?.message)) && deps.shell?.quicklook) {
        notify(`${baseName(address)} is not text.`, [
          ["Quick Look", () => deps.shell.quicklook(address)],
        ]);
      }
      return null;
    }
    // Opened twice in a hurry: the second answer finds the first's buffer.
    const raced = find(opened.path);
    if (raced) {
      activate(raced);
      return raced;
    }
    const buffer = makeBuffer(opened);
    buffers.push(buffer);
    activate(buffer);
    say(opened.mode === "edit" ? `Opened ${local(opened.path)}` : `Opened ${local(opened.path)}, read-only`);
    return buffer;
  }

  function newBuffer() {
    const buffer = makeBuffer({ name: "untitled", text: "", mode: "edit" });
    buffers.push(buffer);
    activate(buffer);
    view.focus();
    return buffer;
  }

  async function closeBuffer(buffer) {
    if (isDirty(buffer)) {
      const ok = await confirm(`Close ${buffer.name} and lose its unsaved changes?`);
      if (!ok) return false;
    }
    const index = buffers.indexOf(buffer);
    if (index < 0) return false;
    buffers.splice(index, 1);
    if (active === buffer) {
      active = null;
      const next = buffers[index] ?? buffers[index - 1] ?? null;
      if (next) activate(next);
      else {
        view.setState(cm.EditorState.create({ doc: "" }));
        clearNotice();
        drawTabs();
        drawState();
      }
    } else {
      drawTabs();
    }
    remember();
    return true;
  }

  // -- saving ---------------------------------------------------------------

  async function save({ force = false } = {}) {
    const b = active;
    if (!b || b.mode !== "edit") return false;
    if (b.saving) return false;
    // Saving anyway after a new file's conflict goes where it was aimed.
    let path = b.path ?? (force ? (b.conflict?.path ?? null) : null);
    if (!path) {
      const answer = await ask("Save as (a path in the working directory)", "/untitled.txt");
      // Typed by the operator, where stray spaces around it are a slip.
      path = normalizePath(typeof answer === "string" ? answer.trim() : answer);
      if (!path) return false;
    }
    const sent = view.state.doc;
    const params = { path, content: sent.toString() };
    if (b.path && b.version) params.expected = b.version;
    if (force) params.force = true;

    const bytes = frameBytes(params);
    if (bytes > b.saveLimit) {
      say(
        `Not saved: ${sizeText(bytes)} is more than a save can carry from the browser ` +
          `(${sizeText(b.saveLimit)}). Split the file, or edit it from the terminal.`,
        "error",
      );
      return false;
    }

    b.saving = true;
    say("Saving…");
    let result;
    try {
      result = await bus.invoke("editor.save", params);
    } catch (error) {
      fail(error);
      return false;
    } finally {
      b.saving = false;
    }

    if (result.status === "conflict") {
      b.conflict = { ...result, path };
      if (b === active) drawNotice();
      say("Not saved: the file on disk is not the one you opened.", "error");
      return false;
    }
    b.path = result.path ?? path;
    b.name = baseName(b.path);
    b.version = result.version ?? null;
    b.modified = result.modified ?? null;
    b.size = result.size ?? null;
    b.saved = sent;
    b.conflict = null;
    b.external = null;
    if (b === active) {
      // A new file gets its language now that it has a name.
      view.dispatch({ effects: langs.reconfigure(languageFor(b.name, cm) ?? []) });
      b.state = view.state;
      drawNotice();
    }
    drawTabs();
    drawState();
    remember();
    say(isDirty(b) ? `Saved ${b.name}; newer edits are not saved yet` : `Saved ${b.name}`);
    return true;
  }

  function notifyConflict(b) {
    const c = b.conflict;
    // A buffer that has a file can take that file's text. A new one has
    // nothing to reload; what is there is another file, which it can open.
    const theirs = !c.version
      ? []
      : b.path
        ? [["Reload theirs", () => revert(b, { quiet: true })]]
        : [["Open it", () => open(c.path)]];
    notify(`${b.name}: ${c.message ?? "the file changed on disk."}`, [
      ["Save anyway", () => save({ force: true })],
      ...theirs,
      [
        "Keep editing",
        () => {
          b.conflict = null;
        },
      ],
    ]);
  }

  function notifyExternal(b) {
    notify(`${b.name} changed on disk, and you have unsaved changes.`, [
      ["Reload theirs", () => revert(b, { quiet: true })],
      [
        "Keep mine",
        () => {
          b.external = null;
        },
      ],
    ]);
  }

  /**
   * Replace a buffer's text with what is on disk, as one undoable change, so
   * a revert never costs the history that led to it.
   */
  async function revert(b = active, { quiet = false, ifClean = false } = {}) {
    if (!b || !b.path) return false;
    if (!quiet && isDirty(b)) {
      const ok = await confirm(`Throw away your changes to ${b.name} and reload it from disk?`);
      if (!ok) return false;
    }
    let opened;
    try {
      opened = await bus.invoke("editor.revert", { path: b.path });
    } catch (error) {
      fail(error);
      return false;
    }
    // A reload decided on because the buffer was clean, by a poll: typing
    // since that decision is the operator's, and a reload would erase it
    // and call the result saved. Ask instead.
    if (ifClean && isDirty(b)) {
      b.external = { modified: opened.modified ?? null, size: opened.size ?? null };
      if (b === active) drawNotice();
      return false;
    }
    replaceText(b, opened.text ?? "");
    b.version = opened.version ?? null;
    b.modified = opened.modified ?? null;
    b.size = opened.size ?? null;
    b.window = opened.window ?? null;
    b.reason = opened.reason ?? null;
    b.conflict = null;
    b.external = null;
    if (opened.mode !== b.mode) {
      b.mode = opened.mode;
      reconfigure(b, editable, b.mode === "edit" ? [] : [cm.EditorState.readOnly.of(true), cm.EditorView.editable.of(false)]);
    }
    b.saved = (b === active ? view.state : b.state).doc;
    if (b === active) drawNotice();
    drawTabs();
    drawState();
    remember();
    say(`Reloaded ${b.name} from disk`);
    return true;
  }

  function replaceText(b, text) {
    const state = b === active ? view.state : b.state;
    const head = Math.min(state.selection.main.head, text.length);
    // Its own history event, never merged with the typing just before it:
    // undoing a reload must give back exactly what the reload replaced.
    const spec = {
      changes: { from: 0, to: state.doc.length, insert: text },
      selection: { anchor: head },
      annotations: cm.isolateHistory.of("full"),
    };
    if (b === active) {
      view.dispatch(spec);
      b.state = view.state;
    } else {
      b.state = state.update(spec).state;
    }
  }

  function reconfigure(b, compartment, value) {
    if (b === active) {
      view.dispatch({ effects: compartment.reconfigure(value) });
      b.state = view.state;
    } else {
      b.state = b.state.update({ effects: compartment.reconfigure(value) }).state;
    }
  }

  // -- big files ------------------------------------------------------------

  async function loadEarlier(b = active) {
    if (!b || !b.window || b.window.start <= 0) return false;
    const have = b.window.end - b.window.start;
    if (have >= WINDOW_TOTAL_MAX) {
      say(`${sizeText(have)} of ${b.name} is loaded, which is as much as this view holds.`, "error");
      return false;
    }
    let part;
    try {
      part = await bus.invoke("editor.open", { path: b.path, before: b.window.start });
    } catch (error) {
      fail(error);
      return false;
    }
    const text = part.text ?? "";
    const state = b === active ? view.state : b.state;
    const head = state.selection.main.head + text.length;
    const spec = { changes: { from: 0, insert: text }, selection: { anchor: head } };
    if (b === active) {
      view.dispatch(spec);
      b.state = view.state;
    } else {
      b.state = state.update(spec).state;
    }
    b.saved = b.state.doc;
    b.window = { start: part.window?.start ?? 0, end: b.window.end };
    drawState();
    say(`Showing ${sizeText(b.window.end - b.window.start)} of ${b.name}`);
    return true;
  }

  // -- watching the disk ----------------------------------------------------

  async function check(b) {
    if (!b.path || b.saving || b.window) return;
    let info;
    try {
      info = await bus.invoke("files.stat", { path: b.path });
    } catch {
      // Gone, or the node is not answering. A save will say which.
      return;
    }
    const moved = info.modified !== b.modified || info.size !== b.size;
    if (!moved) return;
    if (!isDirty(b)) {
      if (await revert(b, { quiet: true, ifClean: true })) {
        say(`${b.name} changed on disk and was reloaded`);
      }
      return;
    }
    if (!b.external) {
      b.external = { modified: info.modified, size: info.size };
      if (b === active) drawNotice();
    }
  }

  async function checkAll() {
    for (const b of [...buffers]) await check(b);
  }

  // -- the session on this device -------------------------------------------

  let rememberTimer = null;
  let restoring = false;
  function rememberSoon() {
    rememberTimer?.();
    rememberTimer = later(400, remember);
  }

  function remember() {
    rememberTimer?.();
    rememberTimer = null;
    // A restore in progress has opened some of the session and not the rest;
    // writing now would forget the rest.
    if (restoring) return;
    let drafts = 0;
    const record = {
      at: now(),
      active: active?.path ?? null,
      buffers: buffers
        .filter((b) => b.path)
        .map((b) => {
          const state = b === active ? view.state : b.state;
          const entry = { path: b.path, cursor: state.selection.main.head };
          if (isDirty(b)) {
            const text = state.doc.toString();
            if (text.length <= DRAFT_MAX) {
              entry.draft = text;
              entry.version = b.version;
              drafts += 1;
            }
          }
          return entry;
        }),
    };
    if (storage.write(sessionKey, JSON.stringify(record)) === false && drafts > 0) {
      say(
        "Unsaved changes could not be kept on this device (its storage is full or blocked); " +
          "they will be lost if the page reloads. Save to keep them.",
        "error",
      );
    }
  }

  async function restore() {
    restoring = true;
    try {
      await restoreSession();
    } finally {
      restoring = false;
    }
    remember();
  }

  async function restoreSession() {
    let record = null;
    try {
      record = JSON.parse(storage.read(sessionKey) ?? "null");
    } catch {
      record = null;
    }
    if (!record || !Array.isArray(record.buffers)) return;
    for (const entry of record.buffers) {
      if (typeof entry?.path !== "string") continue;
      const b = await open(entry.path);
      if (!b) continue;
      if (typeof entry.draft === "string" && b.mode === "edit") {
        replaceText(b, entry.draft);
        // The draft was made against this version, so a save is checked
        // against it: if the file moved on while the tab was closed, the save
        // is a conflict, not an overwrite.
        b.version = typeof entry.version === "string" ? entry.version : b.version;
      }
      const cursor = Math.min(Number(entry.cursor) || 0, (b === active ? view.state : b.state).doc.length);
      if (b === active) view.dispatch({ selection: { anchor: cursor }, scrollIntoView: true });
      else b.state = b.state.update({ selection: { anchor: cursor } }).state;
    }
    const want = record.active ? find(record.active) : null;
    if (want) activate(want);
    drawTabs();
    drawState();
    if (buffers.some(isDirty)) say("Restored unsaved changes from before the reload");
  }

  // -- the file picker ------------------------------------------------------

  async function browse(dir) {
    pickerDir = dir;
    el.pickerPath.value = local(dir) === "/" ? "/" : `${local(dir)}/`;
    let listing;
    try {
      listing = await bus.invoke("files.list", { path: dir });
    } catch (error) {
      el.pickerList.replaceChildren();
      fail(error);
      return;
    }
    const entries = [...(listing.entries ?? [])].sort(
      (a, b) => Number(b.is_dir ?? b.kind === "dir") - Number(a.is_dir ?? a.kind === "dir") ||
        String(a.name).localeCompare(String(b.name)),
    );
    const here = local(listing.path ?? dir);
    const rows = [];
    if (here !== "/") {
      rows.push(row("..", "dir", () => browse(here.slice(0, here.lastIndexOf("/")) || "/")));
    }
    for (const e of entries) {
      const isDir = e.is_dir ?? e.kind === "dir";
      rows.push(
        row(e.name, isDir ? "dir" : "file", () =>
          isDir ? browse(local(e.path)) : pickFile(e.path),
        ),
      );
    }
    el.pickerList.replaceChildren(...rows);
  }

  function row(name, kind, fn) {
    const button = doc.createElement("button");
    button.className = "dj-row";
    button.dataset.kind = kind;
    button.textContent = kind === "dir" ? `${name}/` : name;
    button.addEventListener("click", () => void fn());
    return button;
  }

  async function pickFile(path) {
    closePicker();
    await open(path);
  }

  function openPicker() {
    el.picker.hidden = false;
    void browse(pickerDir);
  }

  function closePicker() {
    el.picker.hidden = true;
  }

  // -- the keyboard ---------------------------------------------------------

  function measureInset() {
    if (!win) return 0;
    try {
      const top = win.parent && win.parent !== win ? win.parent : win;
      const vv = top.visualViewport;
      if (!vv) return 0;
      const frameBottom =
        top === win ? win.innerHeight : win.frameElement?.getBoundingClientRect().bottom;
      return keyboardInset({ frameBottom, viewportTop: vv.offsetTop, viewportHeight: vv.height });
    } catch {
      return 0;
    }
  }

  function fitKeyboard() {
    const inset = measureInset();
    doc.documentElement.style.setProperty("--keyboard-inset", `${inset}px`);
    if (active && view.hasFocus) {
      view.requestMeasure();
      view.dispatch({ effects: cm.EditorView.scrollIntoView(view.state.selection.main.head, { y: "nearest" }) });
    }
  }

  // -- wiring ---------------------------------------------------------------

  el.save.addEventListener("click", () => void save().catch(fail));
  el.undo.addEventListener("click", () => {
    cm.undo(view);
    view.focus();
  });
  el.redo.addEventListener("click", () => {
    cm.redo(view);
    view.focus();
  });
  el.find.addEventListener("click", () => {
    if (cm.searchPanelOpen(view.state)) cm.closeSearchPanel(view);
    else cm.openSearchPanel(view);
  });
  el.gotoLine.addEventListener("click", () => {
    cm.gotoLine(view);
  });
  el.wrap.addEventListener("click", () => setWrap(!wrapOn));
  el.revert.addEventListener("click", () => void revert().catch(fail));
  el.earlier.addEventListener("click", () => void loadEarlier().catch(fail));
  el.open.addEventListener("click", openPicker);
  el.create.addEventListener("click", () => newBuffer());
  el.pickerClose.addEventListener("click", closePicker);
  el.pickerGo.addEventListener("click", () => {
    const value = el.pickerPath.value.trim();
    if (value.endsWith("/")) void browse(normalizePath(value.replace(/\/+$/, "") || "/"));
    else void pickFile(value);
  });
  // Ctrl+S and Cmd+S, wherever focus is. Bound once on the document rather
  // than in CodeMirror's keymap as well, which would save twice.
  const onKey = (event) => {
    if ((event.ctrlKey || event.metaKey) && !event.altKey && event.key.toLowerCase() === "s") {
      event.preventDefault();
      void save().catch(fail);
    }
  };
  doc.addEventListener("keydown", onKey);
  el.pickerPath.addEventListener("keydown", (event) => {
    if (event.key === "Enter") el.pickerGo.click();
  });

  function setWrap(on) {
    wrapOn = on;
    storage.write(WRAP_KEY, on ? "on" : "off");
    for (const b of buffers) reconfigure(b, wrapping, on ? cm.EditorView.lineWrapping : []);
    drawState();
  }

  const stops = [() => doc.removeEventListener("keydown", onKey)];
  stops.push(every(POLL_MS, () => void checkAll()));
  if (win) {
    const onMessage = (event) => {
      // Only the shell that hosts this frame, on this origin.
      if (event.source !== win.parent || event.origin !== win.location.origin) return;
      const data = event.data;
      if (data?.type !== OPEN_MESSAGE) return;
      void open(data.path);
    };
    win.addEventListener("message", onMessage);
    stops.push(() => win.removeEventListener("message", onMessage));
    const onFocus = () => void checkAll();
    win.addEventListener("focus", onFocus);
    stops.push(() => win.removeEventListener("focus", onFocus));
    const onHide = () => {
      if (doc.visibilityState === "hidden") remember();
      else void checkAll();
    };
    doc.addEventListener("visibilitychange", onHide);
    stops.push(() => doc.removeEventListener("visibilitychange", onHide));
    for (const vv of [win.visualViewport, win.parent !== win ? safe(() => win.parent.visualViewport) : null]) {
      if (!vv) continue;
      vv.addEventListener("resize", fitKeyboard);
      vv.addEventListener("scroll", fitKeyboard);
      stops.push(() => {
        vv.removeEventListener("resize", fitKeyboard);
        vv.removeEventListener("scroll", fitKeyboard);
      });
    }
  }

  drawTabs();
  drawState();

  return {
    view,
    buffers,
    get active() {
      return active;
    },
    open,
    newBuffer,
    close: closeBuffer,
    save,
    revert,
    loadEarlier,
    checkAll,
    restore,
    browse,
    setWrap,
    isDirty,
    fitKeyboard,
    /** Open what the page was asked for: the session first, then `?path=`. */
    async start(initialPath) {
      await restore();
      if (initialPath) await open(initialPath);
      if (buffers.length === 0) say("Open a file, or start a new one.");
    },
    destroy() {
      for (const stop of stops) stop();
      rememberTimer?.();
      view.destroy();
    },
  };
}

function safe(fn) {
  try {
    return fn();
  } catch {
    return null;
  }
}
