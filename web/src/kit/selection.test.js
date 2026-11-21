import { describe, expect, it } from "vitest";

import { createSelection } from "../../public/kit/selection.js";

/**
 * The selection rules every app list inherits from `/kit/selection.js`.
 * Each test is a promise a hand on a mouse or a phone relies on.
 */

function sel(keys = ["a", "b", "c", "d", "e"]) {
  const s = createSelection();
  s.setItems(keys);
  return s;
}

describe("with a pointer", () => {
  it("a click selects one thing", () => {
    const s = sel();
    s.click("b");
    s.click("d");
    expect(s.selected).toEqual(["d"]);
  });

  it("Ctrl-click adds and removes without losing the rest", () => {
    const s = sel();
    s.click("b");
    s.click("d", { toggle: true });
    expect(s.selected).toEqual(["b", "d"]);
    s.click("b", { toggle: true });
    expect(s.selected).toEqual(["d"]);
  });

  it("Shift-click selects from the anchor, in list order, whichever way", () => {
    const s = sel();
    s.click("d");
    s.click("b", { extend: true });
    expect(s.selected).toEqual(["b", "c", "d"]);
    // The anchor stays put, so a second Shift-click re-ranges from it.
    s.click("e", { extend: true });
    expect(s.selected).toEqual(["d", "e"]);
  });
});

describe("with the keyboard", () => {
  it("arrows move the cursor and select what they land on", () => {
    const s = sel();
    s.move(1);
    expect(s.cursor).toBe("a");
    s.move(1);
    expect(s.selected).toEqual(["b"]);
    expect(s.cursor).toBe("b");
  });

  it("Shift-arrows extend from the anchor", () => {
    const s = sel();
    s.click("b");
    s.move(1, { extend: true });
    s.move(1, { extend: true });
    expect(s.selected).toEqual(["b", "c", "d"]);
  });

  it("stops at the ends rather than wrapping", () => {
    const s = sel();
    s.click("e");
    s.move(1);
    expect(s.cursor).toBe("e");
    s.move(-100);
    expect(s.cursor).toBe("a");
  });
});

describe("on a phone", () => {
  it("selection mode makes a tap toggle, since there is no Ctrl key", () => {
    const s = sel();
    s.setMode(true);
    s.click("a");
    s.click("c");
    expect(s.selected).toEqual(["a", "c"]);
    s.click("a");
    expect(s.selected).toEqual(["c"]);
  });

  it("leaving selection mode leaves nothing selected", () => {
    const s = sel();
    s.setMode(true);
    s.click("a");
    s.setMode(false);
    expect(s.selected).toEqual([]);
  });
});

describe("when the list changes", () => {
  it("keeps what is still there and drops what is gone", () => {
    const s = sel();
    s.click("b");
    s.click("d", { toggle: true });
    s.setItems(["d", "a", "z"]);
    expect(s.selected).toEqual(["d"]);
  });

  it("does not reselect something that went away and came back", () => {
    // A file deleted and recreated, or a refresh that briefly came back
    // short, is not something the operator chose this time.
    const s = sel(["a", "b"]);
    s.click("b");
    s.setItems(["a"]);
    s.setItems(["a", "b"]);
    expect(s.selected).toEqual([]);
    expect(s.has("b")).toBe(false);
  });

  it("selects by key, so a re-sort does not change what is selected", () => {
    const s = sel(["a", "b", "c"]);
    s.click("b");
    s.setItems(["c", "b", "a"]);
    expect(s.selected).toEqual(["b"]);
  });

  it("forgets a cursor and anchor that are gone", () => {
    const s = sel();
    s.click("c");
    s.setItems(["a", "b"]);
    expect(s.cursor).toBeNull();
    // With no anchor, Shift-click is a plain click rather than a range from
    // a row that no longer exists.
    s.click("b", { extend: true });
    expect(s.selected).toEqual(["b"]);
  });
});

it("reports every change, so a UI can redraw from it", () => {
  let n = 0;
  const s = createSelection({ onChange: () => n++ });
  s.setItems(["a"]);
  s.click("a");
  s.clear();
  expect(n).toBe(3);
});
