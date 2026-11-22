/**
 * The Office app's UI: pages, a reading view, and comments.
 *
 * `mountOffice(document, deps)` builds everything against the bus, shell,
 * storage and window it is handed, so the tests mount the real markup with
 * a fake bus and production mounts it with `/kit/bus.js` (see `main.js`).
 *
 * Built for the isolated frame from the start (docs/APP-UI-ISOLATION.md):
 * no inline script, nothing fetched (`connect-src 'none'`), no content
 * route. Page images arrive as base64 in command results and are shown
 * through `blob:` URLs, which the frame's `img-src` allows.
 *
 * - **Pages** come from LibreOffice on the node, laid out once per document
 *   and drawn lazily a few pages ahead of the scroll position, at the
 *   frame's width times the device pixel ratio. Pinch (or ctrl+wheel) zooms
 *   around the fingers, a double tap fits the width, a sideways swipe or the
 *   arrow keys turn the page.
 * - **Reading** (Word files) reflows at any width. Tracked changes are shown
 *   as changes, with author and date, never accepted. Comments are
 *   highlighted and listed as threads.
 * - **Commenting** writes a new file beside the document,
 *   `Name (commented).docx`; the original is never touched. Further comments
 *   go to that copy (the previous version goes to the trash, so each is
 *   undoable). "Save over original" moves the original to the trash first
 *   and gives the copy its name.
 */

export const AUTHOR_KEY = "deckjack.office.author";
/** The message the shell posts to an open app window to open a file. */
export const OPEN_MESSAGE = "deckjack.app.open";
/** The folders Office was granted; the picker starts at these. */
export const FOLDERS = ["/Documents", "/Downloads"];
/** Pages drawn ahead of and behind what is on screen. */
export const LOOKAHEAD = 3;
export const MIN_ZOOM = 0.5;
export const MAX_ZOOM = 4;
const WORD = /\.(docx|docm)$/i;

/** A working-directory address without the node qualifier the core adds. */
export function local(path) {
  return String(path ?? "").replace(/^[^/:]+:(?=\/)/, "");
}

export function nameOf(path) {
  const p = local(path);
  return p.slice(p.lastIndexOf("/") + 1);
}

export function isWord(path) {
  return WORD.test(String(path ?? ""));
}

export function clampZoom(z) {
  if (!Number.isFinite(z)) return 1;
  return Math.min(MAX_ZOOM, Math.max(MIN_ZOOM, z));
}

/** The zoom a pinch has reached: proportional to the finger spread. */
export function pinchZoom(startZoom, startDistance, distance) {
  if (!(startDistance > 0)) return clampZoom(startZoom);
  return clampZoom(startZoom * (distance / startDistance));
}

/**
 * The scroll offset that keeps the point under the fingers still while the
 * content grows from `from` to `to` times its size.
 */
export function anchoredScroll(scroll, focus, from, to) {
  return (scroll + focus) * (to / from) - focus;
}

/**
 * The pixel width to ask the node for: what the page occupies on screen,
 * times the device pixel ratio, rounded up to the node's cache step and
 * kept inside what it renders.
 */
export function renderWidth(cssWidth, dpr = 1, zoom = 1) {
  const want = Math.ceil((cssWidth * (dpr || 1) * (zoom || 1)) / 100) * 100;
  return Math.min(2400, Math.max(200, want));
}

/** -1 back, 1 forward, 0 not a page swipe. Sideways, quick and long enough. */
export function swipeDirection(dx, dy, ms) {
  if (ms > 700 || Math.abs(dx) < 60 || Math.abs(dx) < 2 * Math.abs(dy)) return 0;
  return dx < 0 ? 1 : -1;
}

/** Comments as threads: each root with its replies, in `order` first. */
export function threads(comments, order = []) {
  // Indexed once: a document can carry tens of thousands of comments, and a
  // search per comment would freeze the frame.
  const ids = new Set(comments.map((c) => c.id));
  const at = new Map();
  order.forEach((id, i) => {
    if (!at.has(id)) at.set(id, i);
  });
  const replies = new Map();
  for (const c of comments) {
    if (!c.reply_to) continue;
    if (!replies.has(c.reply_to)) replies.set(c.reply_to, []);
    replies.get(c.reply_to).push(c);
  }
  const roots = comments
    .map((c, index) => ({ c, index }))
    .filter(({ c }) => !c.reply_to || !ids.has(c.reply_to));
  const rank = ({ c, index }) => (at.has(c.id) ? at.get(c.id) : order.length + index);
  roots.sort((a, b) => rank(a) - rank(b));
  return roots.map(({ c }) => ({ root: c, replies: replies.get(c.id) ?? [] }));
}

/** "Ann Lee, 3 Nov 2026" for a change or a comment. */
export function byline(author, date) {
  const who = author || "Someone";
  if (!date) return who;
  const d = new Date(date);
  if (Number.isNaN(d.getTime())) return who;
  const when = d.toLocaleDateString(undefined, { day: "numeric", month: "short", year: "numeric" });
  return `${who}, ${when}`;
}

const CHANGE_WORD = {
  insertion: "Inserted",
  deletion: "Deleted",
  move_from: "Moved from here",
  move_to: "Moved here",
};

/**
 * The paragraph a selection is in and its UTF-16 offsets into that
 * paragraph's visible text, which is how the node anchors a comment. Markers,
 * placeholders and list numbers are drawn with CSS generated content, so the
 * DOM text of a paragraph is exactly its visible text and a range's string
 * length counts the same units the node does. A selection that runs past
 * its paragraph is cut at that paragraph's end.
 */
export function selectionAnchor(doc, range) {
  if (!range || range.collapsed) return null;
  const paraOf = (node) => {
    const el = node.nodeType === 1 ? node : node.parentElement;
    return el?.closest?.("[data-anchor]") ?? null;
  };
  const para = paraOf(range.startContainer);
  if (!para) return null;
  const before = doc.createRange();
  before.selectNodeContents(para);
  before.setEnd(range.startContainer, range.startOffset);
  const start = before.toString().length;
  let end;
  let clipped = false;
  if (paraOf(range.endContainer) === para) {
    const upto = doc.createRange();
    upto.selectNodeContents(para);
    upto.setEnd(range.endContainer, range.endOffset);
    end = upto.toString().length;
  } else {
    end = para.textContent.length;
    clipped = true;
  }
  if (end <= start) return null;
  return {
    anchor: para.dataset.anchor,
    start,
    end,
    text: para.textContent.slice(start, end),
    clipped,
  };
}

function b64ToBlob(data, mime) {
  const raw = atob(data);
  const bytes = new Uint8Array(raw.length);
  for (let i = 0; i < raw.length; i++) bytes[i] = raw.charCodeAt(i);
  return new Blob([bytes], { type: mime });
}

let jobCounter = 0;

export function mountOffice(doc, deps) {
  const { bus, shell, storage, win } = deps;
  const later =
    deps.later ??
    ((ms, fn) => {
      const t = setTimeout(fn, ms);
      return () => clearTimeout(t);
    });
  const $ = (id) => doc.getElementById(id);
  const els = {
    title: $("title"),
    status: $("status"),
    picker: $("picker"),
    pickerList: $("picker-list"),
    pickerPath: $("picker-path"),
    open: $("open"),
    viewPages: $("view-pages"),
    viewRead: $("view-read"),
    pages: $("pages"),
    column: $("column"),
    reading: $("reading"),
    article: $("article"),
    pageNow: $("page-now"),
    pageCount: $("page-count"),
    prev: $("prev"),
    next: $("next"),
    zoomOut: $("zoom-out"),
    zoomFit: $("zoom-fit"),
    zoomIn: $("zoom-in"),
    exportPdf: $("export"),
    saveOver: $("save-over"),
    commentsToggle: $("comments-toggle"),
    comments: $("comments"),
    commentList: $("comment-list"),
    commentButton: $("comment-button"),
    composer: $("composer"),
    composerQuote: $("composer-quote"),
    composerText: $("composer-text"),
    composerAuthor: $("composer-author"),
    composerSave: $("composer-save"),
    composerCancel: $("composer-cancel"),
    keeps: $("keeps"),
  };

  const state = {
    path: null,
    info: null,
    view: "pages",
    pages: [],
    zoom: 1,
    job: null,
    urls: new Map(),
    reading: null,
    original: null,
    working: null,
    pending: null,
    busy: false,
    armed: false,
    generation: 0,
  };

  function say(text, kind = "info") {
    els.status.textContent = text;
    els.status.dataset.kind = kind;
  }

  function fail(error) {
    say(error instanceof Error ? error.message : String(error), "error");
  }

  // -------------------------------------------------------------------------
  // Opening
  // -------------------------------------------------------------------------

  async function open(path, { keepWorking = false } = {}) {
    const address = local(path);
    if (!address) return;
    cancelJob();
    const generation = ++state.generation;
    state.path = address;
    if (!keepWorking) {
      state.original = address;
      state.working = null;
    }
    els.title.textContent = nameOf(address);
    doc.title = `${nameOf(address)} — Office`;
    hidePicker();
    closeComposer();
    say("Opening…");
    let info;
    try {
      info = await bus.invoke("office.open", { path: address });
    } catch (e) {
      if (generation === state.generation) fail(e);
      return;
    }
    if (generation !== state.generation) return;
    state.info = info;
    const reading = info.reading === true;
    els.viewRead.hidden = !reading;
    els.commentsToggle.hidden = !reading;
    els.keeps.textContent = reading
      ? `${info.keeps}${info.comment?.note ? ` ${info.comment.note}` : ""}`
      : "";
    drawSaveOver();
    setView(reading && state.view === "read" ? "read" : reading ? state.view : "pages");
    const loads = [loadPages(generation)];
    if (reading) loads.push(loadReading(generation));
    await Promise.all(loads);
  }

  function cancelJob() {
    if (state.job) {
      const job = state.job;
      state.job = null;
      bus.invoke("office.cancel", { job }).catch(() => {});
    }
  }

  // -------------------------------------------------------------------------
  // Pages
  // -------------------------------------------------------------------------

  let observer = null;

  function clearPages() {
    observer?.disconnect();
    observer = null;
    for (const url of state.urls.values()) URL.revokeObjectURL(url);
    state.urls.clear();
    state.pages = [];
    els.column.replaceChildren();
  }

  async function loadPages(generation) {
    clearPages();
    if (state.info && state.info.engine === false) {
      els.column.innerHTML = "";
      const p = doc.createElement("p");
      p.className = "dj-empty";
      p.textContent =
        "This node has no LibreOffice, so there are no pages to show. Office needs a node with LibreOffice installed.";
      els.column.append(p);
      if (!state.info.reading) say("No page view on this node.", "error");
      return;
    }
    const job = `pages-${++jobCounter}`;
    state.job = job;
    try {
      const result = await bus.invoke("office.pages", { path: state.path, job }, (p) => {
        if (generation === state.generation && p?.message) say(`${p.message}…`);
      });
      if (generation !== state.generation) return;
      state.job = null;
      state.pages = (result.sizes ?? []).map((size, i) => ({ n: i + 1, size, width: 0, el: null, img: null }));
      els.pageCount.textContent = String(state.pages.length);
      els.pageNow.max = String(state.pages.length);
      drawPages();
      say(`${state.pages.length} page${state.pages.length === 1 ? "" : "s"}`);
    } catch (e) {
      if (generation === state.generation) {
        state.job = null;
        fail(e);
      }
    }
  }

  function drawPages() {
    els.column.replaceChildren();
    els.column.style.setProperty("--zoom", String(state.zoom));
    for (const page of state.pages) {
      const el = doc.createElement("div");
      el.className = "page";
      el.dataset.page = String(page.n);
      el.style.aspectRatio = `${page.size.width} / ${page.size.height}`;
      const img = doc.createElement("img");
      img.alt = `Page ${page.n}`;
      img.draggable = false;
      el.append(img);
      els.column.append(el);
      page.el = el;
      page.img = img;
    }
    // Injectable (and switchable off) for tests; otherwise the window's.
    const IO = "IntersectionObserver" in deps ? deps.IntersectionObserver : win?.IntersectionObserver;
    if (IO) {
      observer = new IO(
        (entries) => {
          for (const entry of entries) {
            if (entry.isIntersecting) void drawPage(Number(entry.target.dataset.page));
          }
        },
        { root: els.pages, rootMargin: `${LOOKAHEAD * 100}% 0px` },
      );
      for (const page of state.pages) observer.observe(page.el);
    } else {
      for (const page of state.pages.slice(0, LOOKAHEAD + 1)) void drawPage(page.n);
    }
  }

  function pageCssWidth() {
    const column = els.pages.clientWidth || 800;
    return Math.max(200, column - 32) * state.zoom;
  }

  async function drawPage(n) {
    const page = state.pages[n - 1];
    if (!page) return;
    const want = renderWidth(pageCssWidth(), win?.devicePixelRatio ?? 1);
    // Already drawn at least this sharp.
    if (page.width >= want || page.loading >= want) return;
    page.loading = want;
    const generation = state.generation;
    try {
      const result = await bus.invoke("office.page.render", { path: state.path, page: n, width: want });
      if (generation !== state.generation) return;
      const url = URL.createObjectURL(b64ToBlob(result.data, result.mime));
      const old = state.urls.get(n);
      state.urls.set(n, url);
      page.img.src = url;
      page.width = result.width;
      if (old) URL.revokeObjectURL(old);
    } catch (e) {
      if (generation === state.generation) fail(e);
    } finally {
      page.loading = 0;
    }
  }

  function currentPage() {
    const top = els.pages.scrollTop;
    let best = 1;
    for (const page of state.pages) {
      if (page.el && page.el.offsetTop - 8 <= top) best = page.n;
    }
    return best;
  }

  function goToPage(n) {
    const page = state.pages[Math.min(state.pages.length, Math.max(1, n)) - 1];
    if (!page?.el) return;
    els.pages.scrollTop = page.el.offsetTop;
    els.pageNow.value = String(page.n);
    void drawPage(page.n);
  }

  function setZoom(z, focusX = els.pages.clientWidth / 2, focusY = els.pages.clientHeight / 2) {
    const from = state.zoom;
    const to = clampZoom(z);
    if (to === from) return;
    state.zoom = to;
    els.column.style.setProperty("--zoom", String(to));
    els.pages.scrollLeft = anchoredScroll(els.pages.scrollLeft, focusX, from, to);
    els.pages.scrollTop = anchoredScroll(els.pages.scrollTop, focusY, from, to);
    els.zoomFit.textContent = `${Math.round(to * 100)}%`;
    settleZoom();
  }

  let settle = null;
  function settleZoom() {
    // Sharper images once the fingers stop, for the pages on screen.
    settle?.();
    settle = later(250, () => {
      const now = currentPage();
      for (let n = Math.max(1, now - 1); n <= Math.min(state.pages.length, now + LOOKAHEAD); n++) {
        void drawPage(n);
      }
    });
  }

  // Touch: two fingers pinch, one finger swipes sideways to turn the page.
  const pointers = new Map();
  let pinch = null;
  let swipe = null;
  let lastTap = 0;

  els.pages.addEventListener("pointerdown", (e) => {
    pointers.set(e.pointerId, { x: e.clientX, y: e.clientY });
    if (pointers.size === 2) {
      const [a, b] = [...pointers.values()];
      pinch = { zoom: state.zoom, distance: Math.hypot(a.x - b.x, a.y - b.y) };
      swipe = null;
    } else if (pointers.size === 1) {
      swipe = { x: e.clientX, y: e.clientY, t: e.timeStamp };
    }
  });
  els.pages.addEventListener("pointermove", (e) => {
    if (!pointers.has(e.pointerId)) return;
    pointers.set(e.pointerId, { x: e.clientX, y: e.clientY });
    if (pinch && pointers.size === 2) {
      const [a, b] = [...pointers.values()];
      const rect = els.pages.getBoundingClientRect();
      setZoom(
        pinchZoom(pinch.zoom, pinch.distance, Math.hypot(a.x - b.x, a.y - b.y)),
        (a.x + b.x) / 2 - rect.left,
        (a.y + b.y) / 2 - rect.top,
      );
      e.preventDefault();
    }
  });
  const lift = (e) => {
    pointers.delete(e.pointerId);
    if (pointers.size < 2) pinch = null;
    if (swipe && pointers.size === 0) {
      const dir = state.zoom <= 1 ? swipeDirection(e.clientX - swipe.x, e.clientY - swipe.y, e.timeStamp - swipe.t) : 0;
      const moved = Math.hypot(e.clientX - swipe.x, e.clientY - swipe.y);
      if (dir) goToPage(currentPage() + dir);
      else if (moved < 10) {
        if (e.timeStamp - lastTap < 300) setZoom(1);
        lastTap = e.timeStamp;
      }
      swipe = null;
    }
  };
  els.pages.addEventListener("pointerup", lift);
  els.pages.addEventListener("pointercancel", lift);
  els.pages.addEventListener(
    "wheel",
    (e) => {
      if (!e.ctrlKey) return;
      e.preventDefault();
      const rect = els.pages.getBoundingClientRect();
      setZoom(state.zoom * Math.exp(-e.deltaY / 300), e.clientX - rect.left, e.clientY - rect.top);
    },
    { passive: false },
  );
  els.pages.addEventListener("scroll", () => {
    els.pageNow.value = String(currentPage());
  });

  // -------------------------------------------------------------------------
  // Reading
  // -------------------------------------------------------------------------

  async function loadReading(generation) {
    els.article.replaceChildren();
    try {
      const result = await bus.invoke("office.text", { path: state.path, only: "blocks" });
      if (generation !== state.generation) return;
      state.reading = result;
      drawReading();
    } catch (e) {
      if (generation === state.generation) fail(e);
    }
  }

  function drawReading() {
    const r = state.reading;
    els.article.replaceChildren(...blocksDom(r.blocks ?? [], r.changes ?? []));
    drawComments();
  }

  function blocksDom(blocks, changes) {
    const out = [];
    for (const block of blocks) {
      if (block.type === "paragraph") out.push(paragraphDom(block, changes));
      else if (block.type === "table") {
        const table = doc.createElement("table");
        for (const row of block.rows) {
          const tr = doc.createElement("tr");
          for (const cell of row) {
            const td = doc.createElement("td");
            if (cell.span > 1) td.colSpan = cell.span;
            td.append(...blocksDom(cell.blocks, changes));
            tr.append(td);
          }
          table.append(tr);
        }
        const wrap = doc.createElement("div");
        wrap.className = "table";
        wrap.append(table);
        out.push(wrap);
      }
    }
    return out;
  }

  function paragraphDom(p, changes) {
    const tag = p.heading ? `h${Math.min(6, p.heading)}` : "p";
    const el = doc.createElement(tag);
    el.className = "para";
    el.dataset.anchor = p.anchor;
    if (p.list) {
      el.classList.add("item");
      el.style.setProperty("--level", String(p.list.level));
      el.dataset.marker = p.list.marker || "•";
    }
    for (const inline of p.inlines) {
      if (inline.type === "text") {
        const change = inline.change != null ? changes[inline.change] : null;
        const kind = change?.kind;
        const tagName = kind === "insertion" || kind === "move_to" ? "ins" : kind === "deletion" || kind === "move_from" ? "del" : "span";
        const span = doc.createElement(tagName);
        span.textContent = inline.text;
        if (change) span.title = `${CHANGE_WORD[kind] ?? "Changed"} by ${byline(change.author, change.date)}`;
        if (inline.comments?.length) {
          span.classList.add("commented");
          span.dataset.comments = inline.comments.join(" ");
        }
        el.append(span);
      } else if (inline.type === "object") {
        const span = doc.createElement("span");
        span.className = "object";
        span.dataset.what = inline.what;
        span.contentEditable = "false";
        el.append(span);
      } else if (inline.type === "comment_mark") {
        const span = doc.createElement("span");
        span.className = "mark";
        span.dataset.comments = inline.id;
        el.append(span);
      }
    }
    return el;
  }

  function drawComments() {
    const r = state.reading;
    const list = threads(r?.comments ?? [], r?.anchored ?? []);
    const count = (r?.comments ?? []).length;
    els.commentsToggle.textContent = count ? `Comments (${count})` : "Comments";
    els.commentList.replaceChildren();
    if (!list.length) {
      const p = doc.createElement("p");
      p.className = "dj-empty";
      p.textContent = "No comments yet. Select text in the reading view to add one.";
      els.commentList.append(p);
      return;
    }
    const canComment = state.info?.comment?.ok === true;
    for (const { root, replies } of list) {
      const thread = doc.createElement("section");
      thread.className = "thread";
      thread.dataset.id = root.id;
      for (const c of [root, ...replies]) {
        const item = doc.createElement("div");
        item.className = c === root ? "comment" : "comment reply";
        const who = doc.createElement("div");
        who.className = "who";
        who.textContent = byline(c.author, c.date) + (c.done ? " · resolved" : "");
        const body = doc.createElement("div");
        body.className = "body";
        body.textContent = c.text;
        item.append(who, body);
        thread.append(item);
      }
      if (canComment) {
        const reply = doc.createElement("button");
        reply.className = "dj-button reply-button";
        reply.textContent = "Reply";
        reply.addEventListener("click", () => openComposer({ reply_to: root.id, quote: `Reply to ${root.author || "comment"}` }));
        thread.append(reply);
      }
      thread.addEventListener("click", (e) => {
        if (e.target.closest(".reply-button")) return;
        highlight(root.id);
      });
      els.commentList.append(thread);
    }
  }

  function highlight(id) {
    for (const el of els.article.querySelectorAll(".lit")) el.classList.remove("lit");
    const hits = [...els.article.querySelectorAll("[data-comments]")].filter((el) =>
      el.dataset.comments.split(" ").includes(id),
    );
    for (const el of hits) el.classList.add("lit");
    if (state.view !== "read") setView("read");
    hits[0]?.scrollIntoView?.({ block: "center" });
  }

  // -------------------------------------------------------------------------
  // Commenting
  // -------------------------------------------------------------------------

  function selectionNow() {
    const sel = win?.getSelection?.() ?? doc.getSelection?.();
    if (!sel || sel.rangeCount === 0) return null;
    const range = sel.getRangeAt(0);
    if (!els.article.contains(range.commonAncestorContainer)) return null;
    return selectionAnchor(doc, range);
  }

  function onSelection() {
    if (state.view !== "read" || state.info?.comment?.ok !== true || !els.composer.hidden) {
      els.commentButton.hidden = true;
      return;
    }
    state.pending = selectionNow();
    els.commentButton.hidden = !state.pending;
  }

  function openComposer(target) {
    state.pending = target;
    els.composerQuote.textContent = target.reply_to
      ? target.quote
      : `“${target.text}”${target.clipped ? " (cut at the end of its paragraph)" : ""}`;
    els.composerText.value = "";
    els.composerAuthor.value = read(AUTHOR_KEY) ?? "";
    els.composer.hidden = false;
    els.commentButton.hidden = true;
    els.composerText.focus?.();
  }

  function closeComposer() {
    els.composer.hidden = true;
    state.pending = null;
  }

  function read(key) {
    try {
      return storage?.read(key) ?? null;
    } catch {
      return null;
    }
  }

  async function saveComment() {
    const target = state.pending;
    const text = els.composerText.value.trim();
    const author = els.composerAuthor.value.trim();
    if (!target || state.busy) return;
    if (!text) return say("Write something first.", "error");
    if (!author) return say("Add your name: it is shown on the comment.", "error");
    try {
      storage?.write(AUTHOR_KEY, author);
    } catch {
      // A remembered name is a convenience.
    }
    const source = state.path;
    const params = { path: source, text, author };
    if (target.reply_to) params.reply_to = target.reply_to;
    else Object.assign(params, { anchor: target.anchor, start: target.start, end: target.end });
    state.busy = true;
    els.composerSave.disabled = true;
    say("Saving the comment…");
    try {
      const result = await bus.invoke("office.comment.add", params);
      const written = local(result.path);
      closeComposer();
      if (state.working && source === state.working) {
        // A newer copy of the working copy: the previous version goes to
        // the trash (undoable) and the new one takes its name.
        try {
          await bus.invoke("files.trash", { paths: [source] });
          await bus.invoke("office.save.inplace", { prepared: written, path: source });
          await open(source, { keepWorking: true });
          say(`Comment saved in ${nameOf(source)}. The previous version is in the trash.`);
        } catch (e) {
          state.working = written;
          await open(written, { keepWorking: true });
          say(`Comment saved in ${nameOf(written)} (${e instanceof Error ? e.message : e}).`, "error");
        }
      } else {
        state.working = written;
        // Said after the reload, which reports its own progress and would
        // otherwise replace this before anyone read it.
        await open(written, { keepWorking: true });
        say(`Comment saved in a copy: ${nameOf(written)}. The original is unchanged.`);
      }
    } catch (e) {
      fail(e);
    } finally {
      state.busy = false;
      els.composerSave.disabled = false;
    }
  }

  function drawSaveOver() {
    els.saveOver.hidden = !(state.working && state.original && state.path === state.working);
  }

  // A second tap confirms, in the frame itself: the isolated frame has no
  // `allow-modals`, so `window.confirm` would never be shown.
  let disarm = null;
  function arm() {
    state.armed = true;
    els.saveOver.textContent = "Tap again: original to trash";
    say(
      `Tap again to move ${nameOf(state.original)} to the trash and give the commented copy its name. You can restore the original from the trash.`,
    );
    disarm?.();
    disarm = later(6000, () => {
      state.armed = false;
      els.saveOver.textContent = "Save over original";
    });
  }

  async function saveOver() {
    const original = state.original;
    const working = state.working;
    if (!original || !working || state.busy) return;
    if (!state.armed) return arm();
    disarm?.();
    state.armed = false;
    els.saveOver.textContent = "Save over original";
    state.busy = true;
    try {
      await bus.invoke("files.trash", { paths: [original] });
      await bus.invoke("office.save.inplace", { prepared: working, path: original });
      state.working = null;
      await open(original);
      say(`Saved over ${nameOf(original)}. The original is in the trash.`);
    } catch (e) {
      fail(e);
    } finally {
      state.busy = false;
    }
  }

  // -------------------------------------------------------------------------
  // Export
  // -------------------------------------------------------------------------

  async function exportPdf() {
    if (!state.path || state.busy) return;
    state.busy = true;
    const job = `export-${++jobCounter}`;
    state.job = job;
    say("Exporting…");
    try {
      const result = await bus.invoke("office.export_pdf", { path: state.path, job });
      say(`Exported ${nameOf(result.path)}.`);
      shell?.quicklook?.(result.path, [result.path]);
    } catch (e) {
      fail(e);
    } finally {
      if (state.job === job) state.job = null;
      state.busy = false;
    }
  }

  // -------------------------------------------------------------------------
  // The picker
  // -------------------------------------------------------------------------

  async function showPicker(path = null) {
    els.picker.hidden = false;
    els.pickerList.replaceChildren();
    if (!path) {
      for (const folder of FOLDERS) els.pickerList.append(pickerRow({ name: folder.slice(1), path: folder, kind: "dir" }));
      els.pickerPath.value = "";
      return;
    }
    els.pickerPath.value = path;
    try {
      const result = await bus.invoke("files.list", { path });
      const parent = path.slice(0, path.lastIndexOf("/"));
      els.pickerList.append(pickerRow({ name: "..", path: FOLDERS.includes(path) ? null : parent, kind: "dir" }));
      for (const entry of result.entries ?? []) {
        els.pickerList.append(pickerRow({ ...entry, path: local(entry.path ?? `${path}/${entry.name}`) }));
      }
    } catch (e) {
      fail(e);
    }
  }

  function pickerRow(entry) {
    const row = doc.createElement("button");
    row.className = "dj-row";
    row.dataset.path = entry.path ?? "";
    const name = doc.createElement("span");
    name.className = "dj-row-name";
    name.textContent = entry.kind === "dir" ? `${entry.name}/` : entry.name;
    row.append(name);
    row.addEventListener("click", () => {
      if (entry.kind === "dir") void showPicker(entry.path);
      else void open(entry.path);
    });
    return row;
  }

  function hidePicker() {
    els.picker.hidden = true;
  }

  // -------------------------------------------------------------------------
  // Views and wiring
  // -------------------------------------------------------------------------

  function setView(view) {
    state.view = view;
    els.pages.hidden = view !== "pages";
    els.reading.hidden = view !== "read";
    els.viewPages.setAttribute("aria-pressed", String(view === "pages"));
    els.viewRead.setAttribute("aria-pressed", String(view === "read"));
    for (const el of [els.prev, els.next, els.pageNow, els.zoomIn, els.zoomOut, els.zoomFit]) el.disabled = view !== "pages";
    els.commentButton.hidden = true;
  }

  els.open.addEventListener("click", () => void showPicker());
  $("picker-close").addEventListener("click", hidePicker);
  $("picker-go").addEventListener("click", () => {
    const p = els.pickerPath.value.trim();
    if (!p) return;
    if (/\.[a-z0-9]{2,5}$/i.test(p)) void open(p);
    else void showPicker(p);
  });
  els.viewPages.addEventListener("click", () => setView("pages"));
  els.viewRead.addEventListener("click", () => setView("read"));
  els.prev.addEventListener("click", () => goToPage(currentPage() - 1));
  els.next.addEventListener("click", () => goToPage(currentPage() + 1));
  els.pageNow.addEventListener("change", () => goToPage(Number(els.pageNow.value)));
  els.zoomIn.addEventListener("click", () => setZoom(state.zoom * 1.25));
  els.zoomOut.addEventListener("click", () => setZoom(state.zoom / 1.25));
  els.zoomFit.addEventListener("click", () => setZoom(1));
  els.exportPdf.addEventListener("click", () => void exportPdf());
  els.saveOver.addEventListener("click", () => void saveOver());
  els.commentsToggle.addEventListener("click", () => {
    els.comments.hidden = !els.comments.hidden;
  });
  els.commentButton.addEventListener("mousedown", (e) => e.preventDefault());
  els.commentButton.addEventListener("click", () => {
    const target = state.pending ?? selectionNow();
    if (target) openComposer(target);
  });
  els.composerSave.addEventListener("click", () => void saveComment());
  els.composerCancel.addEventListener("click", closeComposer);
  doc.addEventListener("selectionchange", onSelection);
  doc.addEventListener("keydown", (e) => {
    if (e.target?.closest?.("input, textarea")) return;
    if (state.view !== "pages") return;
    if (e.key === "ArrowRight" || e.key === "PageDown") {
      goToPage(currentPage() + 1);
      e.preventDefault();
    } else if (e.key === "ArrowLeft" || e.key === "PageUp") {
      goToPage(currentPage() - 1);
      e.preventDefault();
    }
  });

  if (win) {
    win.addEventListener("message", (event) => {
      // Only the shell hosting this frame, on this origin.
      if (event.source !== win.parent || event.origin !== win.location?.origin) return;
      if (event.data?.type !== OPEN_MESSAGE || typeof event.data.path !== "string") return;
      void open(event.data.path);
    });
    // The composer sits above the on-screen keyboard.
    const vv = win.visualViewport;
    if (vv) {
      const fit = () => {
        const inset = Math.max(0, (win.innerHeight ?? 0) - vv.height - (vv.offsetTop ?? 0));
        doc.documentElement.style.setProperty("--keyboard", `${inset}px`);
      };
      vv.addEventListener("resize", fit);
      vv.addEventListener("scroll", fit);
    }
  }

  setView("pages");

  return {
    open,
    state,
    showPicker,
    start(path) {
      if (path) return open(path);
      say("Open a document from Documents or Downloads.");
      return showPicker();
    },
  };
}
