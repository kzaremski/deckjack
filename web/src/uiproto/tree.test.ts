import { describe, expect, it } from "vitest";

import fixture from "./patches.fixture.json";
import { ApplyError, applyPatches, find, type Patch, type UiNode } from "./tree.ts";

/**
 * The shell's patch applier against the core's diff.
 *
 * `patches.fixture.json` is written by `deckjack-protocol`'s
 * `tests/ui_fixture.rs` from the Rust `diff`, and that test fails when the
 * file is stale. So "Rust diffs, TypeScript applies, and lands on the same
 * tree" is checked on cases neither side chose by hand.
 */

interface Case {
  seed: number;
  a: UiNode;
  b: UiNode;
  patches: Patch[];
}

/** No children and an empty list of them are the same node, as in Rust. */
function canonical(node: UiNode): unknown {
  const kids = (node.children ?? []).map(canonical);
  return kids.length ? { key: node.key, widget: node.widget, children: kids } : { key: node.key, widget: node.widget };
}

describe("applying the core's patches", () => {
  it("every generated case lands exactly on the new tree", () => {
    const cases = fixture as unknown as Case[];
    expect(cases.length).toBeGreaterThan(50);
    for (const c of cases) {
      const got = applyPatches(c.a, c.patches);
      expect(canonical(got), `seed ${c.seed}`).toEqual(canonical(c.b));
    }
  });

  it("never modifies the tree it was given, and keeps untouched subtrees by identity", () => {
    const a: UiNode = {
      key: "root",
      widget: { kind: "stack" },
      children: [
        { key: "x", widget: { kind: "text", text: "one" } },
        { key: "y", widget: { kind: "row" }, children: [{ key: "z", widget: { kind: "text", text: "z" } }] },
      ],
    };
    const before = JSON.stringify(a);
    const b = applyPatches(a, [{ op: "props", path: ["x"], widget: { kind: "text", text: "two" } }]);
    expect(JSON.stringify(a)).toBe(before);
    expect(find(b, ["x"])?.widget).toEqual({ kind: "text", text: "two" });
    // The row was not on the patch's path: Vue can skip it.
    expect(find(b, ["y"])).toBe(find(a, ["y"]));
    expect(b).not.toBe(a);
  });

  it("refuses a patch that does not fit, so the channel can ask for the whole tree", () => {
    const a: UiNode = { key: "root", widget: { kind: "stack" }, children: [] };
    expect(() => applyPatches(a, [{ op: "remove", path: [], key: "gone" }])).toThrow(ApplyError);
    expect(() => applyPatches(null, [{ op: "props", path: [], widget: { kind: "stack" } }])).toThrow(
      ApplyError,
    );
    expect(() =>
      applyPatches(a, [{ op: "insert", path: [], index: 3, node: { key: "n", widget: { kind: "stack" } } }]),
    ).toThrow(ApplyError);
  });
});
