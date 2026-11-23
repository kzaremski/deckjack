/**
 * A described list's selection: the app kit's own rules, not a second copy.
 *
 * `public/kit/selection.js` is what every page-serving app's list uses
 * (plain click selects, Ctrl/Cmd toggles, Shift ranges from the anchor, arrow
 * keys move, a touch selection mode toggles, keys survive a refresh). A list
 * the UI protocol describes selects by exactly the same code, so the two
 * kinds of app cannot drift apart while both exist.
 */
import { createSelection as kitSelection, type KitSelection } from "../../public/kit/selection.js";

export type Selection = KitSelection;

export const createSelection: (options?: { onChange?: (s: Selection) => void }) => Selection =
  kitSelection;

/**
 * The app's `selected` last applied to each list's selection, as JSON. Kept
 * beside the selection, which outlives the list's component, so a remount
 * does not reapply an old app value over the operator's own choice.
 */
export const adopted = new WeakMap<Selection, string>();
