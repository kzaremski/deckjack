import type { InjectionKey } from "vue";

import type { UiChannel, UiWindow } from "./channel.ts";
import type { Path } from "./tree.ts";

/**
 * Small pieces the renderer's components share.
 */

/** What every node component below a panel can reach. */
export interface UiContext {
  channel: UiChannel;
  win: UiWindow;
}

export const UiKey: InjectionKey<UiContext> = Symbol("deckjack.uiproto");

/**
 * A path as the string components pass down, because a string compares by
 * value: a parent re-rendering hands an unchanged child the same prop, and
 * Vue skips it. An array would be a new prop every time.
 */
export function childPath(parent: string, key: string): string {
  return parent === "" ? key : `${parent}\u0000${key}`;
}

export function toPath(key: string): Path {
  return key === "" ? [] : key.split("\u0000");
}

/**
 * A `YYYY-MM-DD` or `YYYY-MM-DDTHH:MM` value, in the viewer's locale, and
 * whether it is past by the viewer's clock.
 *
 * A date-only value is past once its day is over, not at midnight that
 * morning: getting this wrong marks everything due today as overdue. (The
 * rule the Tasks page had, kept.)
 */
export function formatDate(
  value: string,
  now: Date = new Date(),
  locale?: string,
): { label: string; past: boolean; valid: boolean } {
  const m = /^(\d{4})-(\d{2})-(\d{2})(?:T(\d{2}):(\d{2}))?$/.exec(value);
  if (!m) return { label: value, past: false, valid: false };
  const [, y, mo, d, hh, mm] = m;
  const timed = hh !== undefined;
  const date = timed
    ? new Date(Number(y), Number(mo) - 1, Number(d), Number(hh), Number(mm))
    : new Date(Number(y), Number(mo) - 1, Number(d));
  const end = timed
    ? date
    : new Date(date.getFullYear(), date.getMonth(), date.getDate(), 23, 59, 59);
  const label = timed
    ? date.toLocaleString(locale, { month: "short", day: "numeric", hour: "numeric", minute: "2-digit" })
    : date.toLocaleDateString(locale, { month: "short", day: "numeric" });
  return { label, past: end < now, valid: true };
}
