/**
 * A selection over a list, for an app's UI.
 *
 * Plain state and plain functions, no DOM: the app draws rows however it
 * likes and calls these from its own event handlers, and the rules that make
 * a list feel like a file manager live in one place instead of in every app.
 *
 * The rules, because each is a promise somebody's hands rely on:
 *
 * - A plain click selects one thing and makes it the anchor.
 * - Ctrl/Cmd-click toggles one thing and moves the anchor to it.
 * - Shift-click selects from the anchor to here, replacing the selection.
 * - Arrow keys move the cursor and select what it lands on; with Shift they
 *   extend from the anchor instead.
 * - **Selection mode** is the touch answer to the missing modifier keys: a
 *   tap toggles, the way Ctrl-click does. Leaving the mode clears the
 *   selection, so a phone never carries a selection it cannot see.
 * - When the list changes (a refresh, a delete), what is still there stays
 *   selected and what is gone is dropped. Nothing is selected by position.
 *
 * Items are identified by key — a path, for Files — never by index, because
 * an index means something else the moment the list is re-sorted.
 */
export function createSelection(options = {}) {
  const onChange = options.onChange ?? (() => {});
  let items = [];
  let selected = new Set();
  let anchor = null;
  let cursor = null;
  let mode = false;

  const indexOf = (key) => (key === null ? -1 : items.indexOf(key));
  const clamp = (i) => Math.max(0, Math.min(items.length - 1, i));

  function range(from, to) {
    const [lo, hi] = from <= to ? [from, to] : [to, from];
    return items.slice(lo, hi + 1);
  }

  function changed() {
    onChange(api);
  }

  const api = {
    /** Replace the list, keeping whatever is still in it. */
    setItems(keys) {
      items = [...keys];
      const present = new Set(items);
      selected = new Set([...selected].filter((k) => present.has(k)));
      if (!present.has(anchor)) anchor = null;
      if (!present.has(cursor)) cursor = null;
      changed();
    },

    /**
     * A click or tap on `key`. `toggle` is Ctrl/Cmd, `extend` is Shift.
     * In selection mode a plain tap toggles.
     */
    click(key, { toggle = false, extend = false } = {}) {
      const i = indexOf(key);
      if (i < 0) return;
      const a = indexOf(anchor);
      if (extend && a >= 0) {
        selected = new Set(range(a, i));
      } else if (toggle || mode) {
        if (selected.has(key)) selected.delete(key);
        else selected.add(key);
        anchor = key;
      } else {
        selected = new Set([key]);
        anchor = key;
      }
      cursor = key;
      changed();
    },

    /** Arrow keys: `delta` is -1 or 1 (or a page). */
    move(delta, { extend = false } = {}) {
      if (items.length === 0) return;
      const from = indexOf(cursor);
      const to = from < 0 ? (delta > 0 ? 0 : items.length - 1) : clamp(from + delta);
      cursor = items[to];
      const a = indexOf(anchor);
      if (extend && a >= 0) {
        selected = new Set(range(a, to));
      } else {
        selected = new Set([cursor]);
        anchor = cursor;
      }
      changed();
    },

    selectAll() {
      selected = new Set(items);
      changed();
    },

    clear() {
      selected = new Set();
      anchor = null;
      changed();
    },

    /** Enter or leave selection mode. Leaving clears. */
    setMode(on) {
      mode = Boolean(on);
      if (!mode) {
        selected = new Set();
        anchor = null;
      }
      changed();
    },

    get mode() {
      return mode;
    },
    /** The selected keys, in list order rather than click order. */
    get selected() {
      return items.filter((k) => selected.has(k));
    },
    has(key) {
      return selected.has(key);
    },
    /** Where the keyboard is: the row Enter and Space act on. */
    get cursor() {
      return cursor;
    },
    get items() {
      return [...items];
    },
  };
  return api;
}
