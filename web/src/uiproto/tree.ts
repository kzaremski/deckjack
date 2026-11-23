/**
 * The UI protocol's tree, as the shell holds it.
 *
 * Mirrors `crates/deckjack-protocol/src/ui.rs`: an app describes its window
 * as a tree of keyed nodes and sends keyed patches; the shell applies them
 * here and draws the result with its own components. Nothing in a tree is
 * ever treated as markup or code: every string is text.
 *
 * Patches are applied **immutably, by path copying**: the nodes on the path
 * from the root to a change are copied and everything else keeps its
 * identity. That is what lets Vue skip every subtree a patch did not touch,
 * so a one-row change in a ten-thousand-row list re-renders one row.
 */

export type Path = string[];

export type Gap = "none" | "small" | "medium" | "large";
export type Tone = "normal" | "muted" | "accent" | "warning" | "danger";

/** A bus command a widget dispatches, as the app's window. */
export interface Action {
  command: string;
  params?: Record<string, unknown>;
  /** Parameter name to field name, read at the moment the action fires. */
  fields?: Record<string, string>;
  /** The parameter that receives the widget's own value. */
  value?: string;
  /** Fields that must not be blank for the action to be enabled. */
  required?: string[];
  /** Fields cleared after the command succeeds, where they still say what was sent. */
  clear?: string[];
}

export interface Choice {
  value: unknown;
  label: string;
}

export type Widget =
  | { kind: "stack"; gap?: Gap; padded?: boolean; grow?: boolean }
  | { kind: "row"; gap?: Gap; padded?: boolean; divider?: boolean; bar?: boolean }
  | { kind: "scroll" }
  | { kind: "text"; text?: string; tone?: Tone; strike?: boolean; small?: boolean; grow?: boolean }
  | { kind: "heading"; text?: string }
  | {
      kind: "button";
      label?: string;
      action?: Action | null;
      enabled?: boolean;
      tone?: Tone;
      quiet?: boolean;
      title?: string | null;
    }
  | {
      kind: "checkbox";
      label?: string;
      show_label?: boolean;
      name?: string | null;
      checked?: boolean;
      action?: Action | null;
      enabled?: boolean;
    }
  | {
      kind: "text_field";
      name?: string;
      value?: string;
      revision?: number;
      placeholder?: string;
      label?: string;
      input?: "text" | "search" | "date";
      secret?: boolean;
      submit?: Action | null;
      grow?: boolean;
    }
  | {
      kind: "select";
      name?: string;
      label?: string;
      options?: Choice[];
      value?: unknown;
      revision?: number;
    }
  | { kind: "list"; label?: string; selection?: "none" | "single" | "multi"; selected?: string[] }
  | { kind: "empty"; title?: string; detail?: string | null }
  | { kind: "progress"; value?: number | null; label?: string }
  | { kind: "date"; value?: string; flag_past?: boolean };

export type WidgetKind = Widget["kind"];

export interface UiNode {
  key: string;
  widget: Widget;
  children?: UiNode[];
}

export type Patch =
  | { op: "replace"; path: Path; node: UiNode }
  | { op: "props"; path: Path; widget: Widget }
  | { op: "insert"; path: Path; index: number; node: UiNode }
  | { op: "remove"; path: Path; key: string }
  | { op: "move"; path: Path; key: string; index: number };

/** What a UI frame from the node carries. */
export type UiBody =
  | { rev: number; ack?: number | null; patches: Patch[] }
  | { status: "lost" }
  | { error: { code: string; message: string; detail?: unknown } };

export class ApplyError extends Error {}

/** A path as one string, for maps keyed by widget. Keys cannot contain NUL. */
export function pathKey(path: Path): string {
  return path.join("\u0000");
}

/** Whether a frame starts the tree over. */
export function isRestart(patches: Patch[]): boolean {
  const first = patches[0];
  return first !== undefined && first.op === "replace" && first.path.length === 0;
}

/** The node at `path`, or undefined. */
export function find(tree: UiNode | null, path: Path): UiNode | undefined {
  let node: UiNode | undefined = tree ?? undefined;
  for (const key of path) {
    node = node?.children?.find((c) => c.key === key);
    if (!node) return undefined;
  }
  return node;
}

/** `fn` applied to the node at `path`, with every node above it copied. */
function update(node: UiNode, path: Path, depth: number, fn: (n: UiNode) => UiNode): UiNode {
  if (depth === path.length) return fn(node);
  const children = node.children ?? [];
  const at = children.findIndex((c) => c.key === path[depth]);
  if (at < 0) throw new ApplyError(`no node at ${JSON.stringify(path)}`);
  const copy = children.slice();
  copy[at] = update(children[at]!, path, depth + 1, fn);
  return { ...node, children: copy };
}

function applyOne(tree: UiNode | null, patch: Patch): UiNode {
  if (patch.op === "replace" && patch.path.length === 0) return patch.node;
  if (!tree) throw new ApplyError("a patch arrived before any tree");
  switch (patch.op) {
    case "replace":
      if (patch.path[patch.path.length - 1] !== patch.node.key) {
        throw new ApplyError(`a replace at ${JSON.stringify(patch.path)} changed the key`);
      }
      return update(tree, patch.path, 0, () => patch.node);
    case "props":
      return update(tree, patch.path, 0, (n) => ({ ...n, widget: patch.widget }));
    case "insert":
      return update(tree, patch.path, 0, (n) => {
        const children = n.children ?? [];
        if (children.some((c) => c.key === patch.node.key)) {
          throw new ApplyError(`a child ${patch.node.key} already exists`);
        }
        if (patch.index < 0 || patch.index > children.length) {
          throw new ApplyError(`index ${patch.index} is past the end`);
        }
        const copy = children.slice();
        copy.splice(patch.index, 0, patch.node);
        return { ...n, children: copy };
      });
    case "remove":
      return update(tree, patch.path, 0, (n) => {
        const children = n.children ?? [];
        const at = children.findIndex((c) => c.key === patch.key);
        if (at < 0) throw new ApplyError(`no child ${patch.key}`);
        const copy = children.slice();
        copy.splice(at, 1);
        return { ...n, children: copy };
      });
    case "move":
      return update(tree, patch.path, 0, (n) => {
        const children = n.children ?? [];
        const at = children.findIndex((c) => c.key === patch.key);
        if (at < 0) throw new ApplyError(`no child ${patch.key}`);
        const copy = children.slice();
        const [moved] = copy.splice(at, 1);
        if (patch.index < 0 || patch.index > copy.length) {
          throw new ApplyError(`index ${patch.index} is past the end`);
        }
        copy.splice(patch.index, 0, moved!);
        return { ...n, children: copy };
      });
  }
}

/**
 * The tree with `patches` applied, in order. The input is never modified.
 * Throws `ApplyError` on a patch that does not fit, which means a frame was
 * lost or a side has a bug; the caller asks for the whole tree again.
 */
export function applyPatches(tree: UiNode | null, patches: Patch[]): UiNode {
  let out = tree;
  for (const patch of patches) out = applyOne(out, patch);
  if (!out) throw new ApplyError("no tree");
  return out;
}

/** Every node, depth first, with its path. Lists' rows are skipped unless asked. */
export function* walk(
  node: UiNode,
  options: { intoLists?: boolean } = {},
  path: Path = [],
): Generator<{ node: UiNode; path: Path }> {
  yield { node, path };
  if (node.widget.kind === "list" && !options.intoLists) return;
  for (const child of node.children ?? []) {
    yield* walk(child, options, [...path, child.key]);
  }
}
