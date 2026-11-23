/// <reference types="vite/client" />

/*
 * `.vue` files are typed by vue-tsc itself, so there is deliberately no
 * `declare module "*.vue"` shim here: a shim would type every component's
 * props as `{}` and silently stop checking the one thing worth checking.
 */

interface KeyboardLockApi {
  lock(keyCodes?: string[]): Promise<void>;
  unlock(): void;
}

/**
 * Keyboard Lock is Chromium-only and absent from lib.dom.d.ts, so it is
 * declared rather than cast away at the call site.
 */
interface Navigator {
  readonly keyboard?: KeyboardLockApi;
}

/**
 * The app kit's selection rules (`public/kit/selection.js`), which the UI
 * protocol's list uses too, so a described list and a page's list select
 * the same way. Plain JavaScript, typed here.
 */
declare module "*/public/kit/selection.js" {
  export interface KitSelection {
    setItems(keys: string[]): void;
    click(key: string, modifiers?: { toggle?: boolean; extend?: boolean }): void;
    move(delta: number, modifiers?: { extend?: boolean }): void;
    selectAll(): void;
    clear(): void;
    setMode(on: boolean): void;
    readonly mode: boolean;
    readonly selected: string[];
    has(key: string): boolean;
    readonly cursor: string | null;
    readonly items: string[];
  }
  export function createSelection(options?: { onChange?: (s: KitSelection) => void }): KitSelection;
}
