/**
 * What the Editor app's vendored CodeMirror bundle exports.
 *
 * App UIs are plain files served from the app binary with no build step at
 * runtime (docs/APP-SUITE.md §2), so CodeMirror reaches the editor as one
 * prebuilt ES module, `crates/deckjack-editor/ui/vendor/codemirror.js`,
 * made from this file by `npm run build:editor-vendor` and committed. A test
 * (`web/src/apps/editor-vendor.test.js`) rebuilds it and fails when the
 * committed copy is stale, so the file in the repository is always what this
 * entry and the locked package versions produce.
 *
 * Only what `editor.js` uses is exported. Language choice by file name is
 * `editor.js`'s, where it is tested; this file only names what exists.
 */

export { EditorState, Compartment, EditorSelection } from "@codemirror/state";
export {
  EditorView,
  keymap,
  lineNumbers,
  highlightActiveLine,
  highlightActiveLineGutter,
  highlightSpecialChars,
  drawSelection,
  dropCursor,
  rectangularSelection,
  crosshairCursor,
} from "@codemirror/view";
export {
  defaultKeymap,
  history,
  historyKeymap,
  undo,
  redo,
  undoDepth,
  isolateHistory,
  redoDepth,
  indentWithTab,
} from "@codemirror/commands";
export {
  search,
  searchKeymap,
  openSearchPanel,
  closeSearchPanel,
  searchPanelOpen,
  gotoLine,
  highlightSelectionMatches,
} from "@codemirror/search";
export {
  syntaxHighlighting,
  indentOnInput,
  bracketMatching,
  foldGutter,
  foldKeymap,
  StreamLanguage,
} from "@codemirror/language";
export { closeBrackets, closeBracketsKeymap } from "@codemirror/autocomplete";
export { classHighlighter } from "@lezer/highlight";

export { javascript } from "@codemirror/lang-javascript";
export { rust } from "@codemirror/lang-rust";
export { python } from "@codemirror/lang-python";
export { json } from "@codemirror/lang-json";
export { markdown } from "@codemirror/lang-markdown";
export { html } from "@codemirror/lang-html";
export { css } from "@codemirror/lang-css";
export { yaml } from "@codemirror/lang-yaml";
export { cpp } from "@codemirror/lang-cpp";
export { go } from "@codemirror/lang-go";
export { java } from "@codemirror/lang-java";
export { sql } from "@codemirror/lang-sql";
export { xml } from "@codemirror/lang-xml";

export { toml } from "@codemirror/legacy-modes/mode/toml";
export { shell } from "@codemirror/legacy-modes/mode/shell";
export { lua } from "@codemirror/legacy-modes/mode/lua";
export { dockerFile } from "@codemirror/legacy-modes/mode/dockerfile";
export { diff } from "@codemirror/legacy-modes/mode/diff";
export { properties } from "@codemirror/legacy-modes/mode/properties";
export { nginx } from "@codemirror/legacy-modes/mode/nginx";
