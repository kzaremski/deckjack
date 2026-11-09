import { ref, type Ref } from "vue";

import type { Risk } from "../protocol.ts";

/**
 * Shell-local interface state: the palette, the switcher and the notices.
 *
 * None of this is server state. A palette left open on the laptop should not
 * open on the phone, so it deliberately never reaches the bus.
 */

export interface Notice {
  id: number;
  kind: "info" | "warn" | "error";
  text: string;
  /** Set when the notice came from a command, so it can name it. */
  command?: string;
}

/** A confirmation the operator has to answer before a command runs. */
export interface Confirmation {
  command: string;
  title: string;
  detail: string;
  risk: Risk;
  /** How many objects it will affect, when the backend reported a count:
   * "an agent deleting much of a researcher's inbox" is the lesson here. */
  count?: number;
  resolve(ok: boolean): void;
}

export interface UiStore {
  paletteOpen: Ref<boolean>;
  switcherOpen: Ref<boolean>;
  notices: Ref<Notice[]>;
  confirmation: Ref<Confirmation | null>;
  notify(kind: Notice["kind"], text: string, command?: string): number;
  dismiss(id: number): void;
  /** Resolves true when the operator accepts. */
  confirm(input: Omit<Confirmation, "resolve">): Promise<boolean>;
}

export interface UiEnv {
  /** Schedules the automatic dismissal of a notice. */
  delay(ms: number, fn: () => void): () => void;
}

export function browserUiEnv(): UiEnv {
  return {
    delay(ms, fn) {
      const handle = window.setTimeout(fn, ms);
      return () => window.clearTimeout(handle);
    },
  };
}

/** How long an informational notice stays up. Errors stay until dismissed. */
const NOTICE_MS = 6000;

export function createUi(env: UiEnv): UiStore {
  const paletteOpen = ref(false);
  const switcherOpen = ref(false);
  const notices = ref<Notice[]>([]);
  const confirmation = ref<Confirmation | null>(null);
  let nextId = 1;

  function dismiss(id: number) {
    notices.value = notices.value.filter((n) => n.id !== id);
  }

  return {
    paletteOpen,
    switcherOpen,
    notices,
    confirmation,
    notify(kind, text, command) {
      const id = nextId++;
      notices.value = [...notices.value, { id, kind, text, command }];
      // An error that disappears on its own is an error nobody read.
      if (kind !== "error") env.delay(NOTICE_MS, () => dismiss(id));
      return id;
    },
    dismiss,
    confirm(input) {
      return new Promise<boolean>((resolve) => {
        confirmation.value = {
          ...input,
          resolve(ok) {
            confirmation.value = null;
            resolve(ok);
          },
        };
      });
    },
  };
}
