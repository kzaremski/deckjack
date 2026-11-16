import { ref, type Ref } from "vue";

/**
 * Keyboard-first operation: the chord table, and the Keyboard Lock request
 * that makes the browser's own chords reach Deckjack instead.
 *
 * Shortcuts are bound to command ids, never to callbacks. That is what keeps
 * "the UI, the keyboard, the palette, the CLI and agents all dispatch the
 * same command" true rather than aspirational, and it is why rebinding is a
 * data change instead of a code change.
 */

/** Keys the browser normally keeps, which the doc specifically wants back. */
export const LOCKED_KEYS = ["KeyW", "KeyT", "Tab", "Escape"] as const;

export type LockStatus = "idle" | "locked" | "unsupported" | "refused";

export interface Shortcut {
  commandId: string;
  /** A chord such as `mod+k`, `mod+shift+enter`, `alt+1`, `escape`. */
  chord: string;
  /**
   * Whether it fires while a text field has focus. Chords with a modifier do
   * by default (mod+K must open the palette from inside a search box); bare
   * keys do not, or typing "n" in a document would open a new window.
   */
  whileTyping?: boolean;
}

export interface KeyboardEnv {
  /** True on macOS and iPadOS, where `mod` means Meta rather than Control. */
  apple: boolean;
  keyboard?: { lock(keys?: string[]): Promise<void>; unlock(): void };
  storage: { read(key: string): string | null; write(key: string, v: string): void };
}

const CHORDS_KEY = "deckjack.shortcuts";

export function browserKeyboardEnv(storage: KeyboardEnv["storage"]): KeyboardEnv {
  return {
    apple: /mac|iphone|ipad|ipod/i.test(navigator.platform || navigator.userAgent),
    keyboard: navigator.keyboard,
    storage,
  };
}

/**
 * The chord an event represents, in the same spelling the table uses.
 *
 * `event.key` rather than `event.code`, so a chord follows the operator's
 * layout: `mod+;` is wherever semicolon is on a Dvorak keyboard, not wherever
 * it is on a US QWERTY one.
 */
export function chordOf(event: KeyboardEvent, apple: boolean): string {
  const parts: string[] = [];
  const mod = apple ? event.metaKey : event.ctrlKey;
  if (mod) parts.push("mod");
  // The other of the two is still reportable, so mod+ctrl stays expressible.
  if (apple ? event.ctrlKey : event.metaKey) parts.push(apple ? "ctrl" : "meta");
  if (event.altKey) parts.push("alt");
  if (event.shiftKey) parts.push("shift");

  let key = event.key.toLowerCase();
  if (key === " ") key = "space";
  if (key === "escape") key = "escape";
  if (!["control", "meta", "alt", "shift"].includes(key)) parts.push(key);
  return parts.join("+");
}

/** How a chord is shown to the operator: `⌘K` on Apple, `Ctrl+K` elsewhere. */
export function chordLabel(chord: string, apple: boolean): string {
  return chord
    .split("+")
    .map((part) => {
      switch (part) {
        case "mod":
          return apple ? "⌘" : "Ctrl";
        case "alt":
          return apple ? "⌥" : "Alt";
        case "shift":
          return apple ? "⇧" : "Shift";
        case "ctrl":
          return apple ? "⌃" : "Ctrl";
        case "meta":
          return apple ? "⌘" : "Win";
        case "escape":
          return "Esc";
        case "arrowleft":
          return "←";
        case "arrowright":
          return "→";
        case "arrowup":
          return "↑";
        case "arrowdown":
          return "↓";
        default:
          return part.length === 1 ? part.toUpperCase() : part[0]!.toUpperCase() + part.slice(1);
      }
    })
    .join(apple ? "" : "+");
}

function isTypingTarget(target: EventTarget | null): boolean {
  if (!(target instanceof HTMLElement)) return false;
  if (target.isContentEditable) return true;
  const tag = target.tagName;
  return tag === "INPUT" || tag === "TEXTAREA" || tag === "SELECT";
}

export interface Keyboard {
  apple: boolean;
  lockStatus: Ref<LockStatus>;
  bind(shortcuts: Shortcut[]): void;
  /** Overrides one binding and remembers it. */
  rebind(commandId: string, chord: string): void;
  chordFor(commandId: string): string | null;
  labelFor(commandId: string): string | null;
  /** Returns the command id to dispatch, or null to let the event through. */
  resolve(event: KeyboardEvent): string | null;
  /**
   * Asks the browser for Ctrl/Cmd+W, Ctrl/Cmd+T, Tab and Escape. Chromium
   * only, and only in an installed window or fullscreen.
   */
  requestLock(): Promise<LockStatus>;
  releaseLock(): void;
}

export function createKeyboard(env: KeyboardEnv): Keyboard {
  const table = new Map<string, Shortcut>();
  /** The chord shown in the UI for a command: the first one the table gave it. */
  const primary = new Map<string, string>();
  /** Every chord currently bound to a command, so a rebind can clear them all. */
  const live = new Map<string, string[]>();
  const lockStatus = ref<LockStatus>("idle");

  let overrides: Record<string, string> = {};
  try {
    const raw = env.storage.read(CHORDS_KEY);
    overrides = raw ? (JSON.parse(raw) as Record<string, string>) : {};
  } catch {
    // A corrupt override file must not cost the operator every shortcut.
    overrides = {};
  }

  /** Unbinds every chord a command holds. */
  function forget(commandId: string) {
    for (const chord of live.get(commandId) ?? []) table.delete(chord);
    live.delete(commandId);
    primary.delete(commandId);
  }

  /**
   * Binds one chord, distinguishing an *alias* from a *rebind*.
   *
   * These pull in opposite directions and conflating them cost the shell its
   * primary chord. One command may legitimately ship several chords -- the
   * table gives `shell.palette.open` both `mod+k` and `mod+shift+p` -- while a
   * chord the operator reassigned must replace what shipped rather than
   * leaving two live. This used to keep exactly one chord per command and
   * delete the previous one, so loading the shipped table made the second
   * palette entry evict the first: `mod+k` was dead, and `labelFor` advertised
   * Ctrl+Shift+P to every surface that displays it.
   *
   * So a default adds to whatever the command already has, and an operator's
   * override replaces the lot. The label follows the first chord installed,
   * which is the one the table lists first and the one worth advertising.
   */
  function install(shortcut: Shortcut) {
    const override = overrides[shortcut.commandId];
    if (override) forget(shortcut.commandId);
    const chord = override ?? shortcut.chord;
    table.set(chord, { ...shortcut, chord });
    const chords = live.get(shortcut.commandId) ?? [];
    if (!chords.includes(chord)) chords.push(chord);
    live.set(shortcut.commandId, chords);
    if (!primary.has(shortcut.commandId)) primary.set(shortcut.commandId, chord);
  }

  return {
    apple: env.apple,
    lockStatus,
    bind(shortcuts) {
      shortcuts.forEach(install);
    },
    rebind(commandId, chord) {
      overrides[commandId] = chord;
      env.storage.write(CHORDS_KEY, JSON.stringify(overrides));
      const existing = table.get(primary.get(commandId) ?? "");
      // Every previous chord goes, aliases included: the operator asked for
      // one chord, not for theirs plus whatever happened to ship.
      forget(commandId);
      // And take the chord off whoever held it, so the table never carries two
      // claims on one key. Without this the loser went on reporting the chord
      // through `chordFor`/`labelFor` -- the shell advertised a key that now
      // belonged to something else -- and rebinding the loser later unbound
      // the winner's shortcut while still advertising it.
      const loser = table.get(chord)?.commandId;
      if (loser && loser !== commandId) {
        const left = (live.get(loser) ?? []).filter((c) => c !== chord);
        if (left.length > 0) {
          live.set(loser, left);
          if (primary.get(loser) === chord) primary.set(loser, left[0]!);
        } else {
          live.delete(loser);
          primary.delete(loser);
        }
      }
      table.set(chord, { commandId, chord, whileTyping: existing?.whileTyping });
      live.set(commandId, [chord]);
      primary.set(commandId, chord);
    },
    chordFor: (commandId) => primary.get(commandId) ?? null,
    labelFor(commandId) {
      const chord = primary.get(commandId);
      return chord ? chordLabel(chord, env.apple) : null;
    },
    resolve(event) {
      if (event.isComposing) return null;
      const chord = chordOf(event, env.apple);
      const hit = table.get(chord);
      if (!hit) return null;
      // Shift alone does not count. It is how a capital letter is typed, so
      // treating it as a modifier let a shift-only chord fire inside a text
      // field -- `chord.includes("+")` was true for "shift+n", and typing an
      // N dispatched a command. Nothing shipped is shift-only, which is
      // exactly why this was a trap for the first one that is.
      const hasModifier = /(^|\+)(mod|ctrl|meta|alt)\+/.test(chord);
      const allowed = hit.whileTyping ?? hasModifier;
      if (!allowed && isTypingTarget(event.target)) return null;
      return hit.commandId;
    },
    async requestLock() {
      if (!env.keyboard) {
        lockStatus.value = "unsupported";
        return lockStatus.value;
      }
      try {
        await env.keyboard.lock([...LOCKED_KEYS]);
        lockStatus.value = "locked";
      } catch {
        // Chromium refuses outside an installed window or fullscreen. Not an
        // error worth surfacing as one: the shell works without it, the
        // browser just keeps Ctrl+W.
        lockStatus.value = "refused";
      }
      return lockStatus.value;
    },
    releaseLock() {
      env.keyboard?.unlock();
      if (lockStatus.value === "locked") lockStatus.value = "idle";
    },
  };
}

/** The shipped table. Every entry is a command id that exists on the bus. */
export const DEFAULT_SHORTCUTS: Shortcut[] = [
  { commandId: "shell.palette.open", chord: "mod+k" },
  { commandId: "shell.palette.open", chord: "mod+shift+p" },
  { commandId: "shell.window.close", chord: "mod+w" },
  { commandId: "shell.window.next", chord: "mod+tab" },
  { commandId: "shell.window.previous", chord: "mod+shift+tab" },
  { commandId: "shell.window.snap.left", chord: "mod+alt+arrowleft" },
  { commandId: "shell.window.snap.right", chord: "mod+alt+arrowright" },
  { commandId: "shell.window.maximize", chord: "mod+alt+arrowup" },
  { commandId: "shell.window.minimize", chord: "mod+alt+arrowdown" },
  { commandId: "shell.windows.tile", chord: "mod+alt+t" },
  { commandId: "shell.mode.toggle", chord: "mod+alt+m" },
  { commandId: "shell.theme.cycle", chord: "mod+alt+d" },
  { commandId: "shell.split.toggle", chord: "mod+alt+s" },
  { commandId: "shell.switcher.open", chord: "mod+j" },
  // `system.lockdown`, not `shell.lockdown`: the lockdown is the node's, not
  // the shell's, and this chord was bound to an id nothing registers, so the
  // panic kill-switch silently dispatched nothing. TopBar's button used the
  // right id all along, which is how the mismatch stayed hidden.
  { commandId: "system.lockdown", chord: "mod+alt+l" },
];
