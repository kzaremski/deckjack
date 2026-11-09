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
