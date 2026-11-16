// @vitest-environment happy-dom
import { describe, expect, it } from "vitest";

import { createCommands, riskLabel, type CommandEntry, type CommandsEnv } from "./commands.ts";
import { chordOf, createKeyboard, DEFAULT_SHORTCUTS } from "../shell/keyboard.ts";
import { harness } from "../testing/shell-harness.ts";
import type { CommandSpec, Risk } from "../protocol.ts";

/**
 * One promise, two modules: the operator asked for something, and the right
 * thing happened.
 *
 * "Everything is a command" only holds if the registry and the chord table
 * agree with each other and with the shell that owns both. So these tests ask
 * the questions an operator would ask — did the palette find it, did the
 * shortcut run it, did my rebinding stick, did a reconnect quietly take my
 * shell commands away — and never how the registry stores a list or how the
 * chord table is keyed. Scores in particular are an implementation detail: the
 * promise is an *order*, so the tests assert order and never a number.
 *
 * Several tests drive the real shell from `shell-harness.ts` rather than a
 * freshly built store. The defect this project keeps producing is a module that
 * is complete, unit-tested and never wired up, and a store-level test cannot
 * see that. Asking the constructed shell to resolve a chord and dispatch what
 * it resolved tests the seam the operator actually touches.
 */

function memoryStorage(seed: Record<string, string> = {}) {
  const map = new Map<string, string>(Object.entries(seed));
  return {
    map,
    env: {
      read: (k: string) => map.get(k) ?? null,
      write: (k: string, v: string) => void map.set(k, v),
    } satisfies CommandsEnv["storage"],
  };
}

/** A command that records whether it ran, which is all most tests need. */
function entry(id: string, title: string, extra: Partial<CommandEntry> = {}): CommandEntry {
  return { id, title, risk: "read_only", run: () => {}, ...extra };
}

function titles(scored: Array<{ entry: CommandEntry }>): string[] {
  return scored.map((s) => s.entry.id);
}

/** A keyboard event as the browser would deliver it for a chord. */
function press(init: KeyboardEventInit): KeyboardEvent {
  return new KeyboardEvent("keydown", init);
}

/** A focused text field: an input unless a different tag is asked for. */
function textField(tag: "input" | "textarea" = "input"): HTMLElement {
  const field = document.createElement(tag);
  document.body.appendChild(field);
  return field;
}

/**
 * The same keystroke, but delivered to a text field. `target` is read-only on
 * a constructed event, so it is defined rather than assigned.
 */
function pressInto(field: HTMLElement, init: KeyboardEventInit): KeyboardEvent {
  const event = press(init);
  Object.defineProperty(event, "target", { value: field });
  return event;
}

describe("the palette finding what the operator half-remembers", () => {
  it("matches the letters of a name in order, not just a prefix of it", () => {
    const commands = createCommands({ storage: memoryStorage().env });
    commands.register([
      entry("app.open.files", "Open Files"),
      entry("shell.theme.cycle", "Next theme"),
    ]);

    const found = commands.search("opfl");

    expect(titles(found), "a subsequence of the title did not find the command").toContain(
      "app.open.files",
    );
  });

  it("puts the command the letters actually name above one they only happen to fit", () => {
    const commands = createCommands({ storage: memoryStorage().env });
    commands.register([
      // Registered in the unhelpful order on purpose: if ranking does nothing,
      // the near-miss stays on top and this test fails.
      entry("shell.toggle.labels", "Toggle this later, eventually"),
      entry("shell.windows.tile", "Tile windows side by side"),
    ]);

    const found = commands.search("tile");

    expect(titles(found)[0], "the best match was not offered first").toBe("shell.windows.tile");
  });

  it("gives the palette no more rows than it asked to draw", () => {
    const commands = createCommands({ storage: memoryStorage().env });
    commands.register([
      entry("app.open.files", "Open Files"),
      entry("app.open.notes", "Open Notes"),
      entry("app.open.mail", "Open Mail"),
      entry("app.open.music", "Open Music"),
    ]);

    expect(commands.search("open", 2).length).toBe(2);
    expect(commands.search("", 2).length, "an empty query ignored the limit").toBe(2);
  });

  it("offers what the operator just used before what they have never used", async () => {
    const commands = createCommands({ storage: memoryStorage().env });
    commands.register([
      entry("app.open.files", "Open Files"),
      entry("app.open.notes", "Open Notes"),
      entry("app.open.mail", "Open Mail"),
    ]);

    await commands.dispatch("app.open.mail");

    expect(titles(commands.search(""))[0], "the last command used was not offered first").toBe(
      "app.open.mail",
    );
  });

  it("breaks a tie between two equally good matches with what was used last", async () => {
    // Both titles match "open" identically and are the same length, so nothing
    // but recency can separate them. Without the recency bonus a stable sort
    // leaves them in registration order and Files wins.
    const commands = createCommands({ storage: memoryStorage().env });
    commands.register([entry("app.open.files", "Open Files"), entry("app.open.notes", "Open Notes")]);

    await commands.dispatch("app.open.notes");

    expect(titles(commands.search("open"))[0], "recency did not break the tie").toBe(
      "app.open.notes",
    );
  });

  it("hides a command that says it cannot run right now", () => {
    const commands = createCommands({ storage: memoryStorage().env });
    commands.register([
      entry("shell.window.close", "Close window", { available: () => false }),
      entry("app.open.files", "Open Files"),
    ]);

    expect(titles(commands.search("close"))).not.toContain("shell.window.close");
    expect(titles(commands.search("")), "an unavailable command was offered").not.toContain(
      "shell.window.close",
    );
  });
});

describe("the operator's recent commands across a reload", () => {
  it("are still in front of them after the shell is rebuilt", async () => {
    const storage = memoryStorage();
    const first = createCommands({ storage: storage.env });
    first.register([entry("app.open.files", "Open Files"), entry("app.open.mail", "Open Mail")]);
    await first.dispatch("app.open.mail");

    // The same storage, a new store: what a reload looks like from here.
    const rebuilt = createCommands({ storage: storage.env });
    rebuilt.register([entry("app.open.files", "Open Files"), entry("app.open.mail", "Open Mail")]);

    expect(titles(rebuilt.search(""))[0], "recents did not survive a reload").toBe("app.open.mail");
  });

  it("cost nothing when what was stored is not readable", () => {
    const storage = memoryStorage({ "deckjack.recentCommands": "{not json at all" });

    const commands = createCommands({ storage: storage.env });
    commands.register([entry("app.open.files", "Open Files")]);

    // The promise is that the palette still works, not that anything is
    // recovered: corrupt history is worth exactly as much as no history.
    expect(titles(commands.search("files")), "corrupt history broke the palette").toContain(
      "app.open.files",
    );
    expect(titles(commands.search("")).length).toBe(1);
  });
});

describe("a node reconnecting and re-advertising its commands", () => {
  it("does not take away the commands that live in the shell", async () => {
    const rig = harness();
    const spec: CommandSpec = { id: "files.read", title: "Read a file", risk: "read_only" };
    rig.shell.commands.adopt([spec], async () => {});

    // The shell's own commands are registered locally by the real shell, so
    // this asserts on the registry the operator's palette actually reads.
    expect(
      rig.shell.commands.byId("shell.palette.open"),
      "a reconnect deleted the shell's own commands",
    ).toBeTruthy();
    await rig.shell.commands.dispatch("shell.palette.open");
    expect(rig.shell.ui.paletteOpen.value, "a shell command stopped working after adopt").toBe(true);
  });

  it("replaces the commands the previous connection advertised", () => {
    const rig = harness();
    const gone: CommandSpec = { id: "files.read", title: "Read a file", risk: "read_only" };
    const now: CommandSpec = { id: "mail.send", title: "Send mail", risk: "outbound" };

    rig.shell.commands.adopt([gone], async () => {});
    rig.shell.commands.adopt([now], async () => {});

    expect(rig.shell.commands.byId("mail.send")).toBeTruthy();
    expect(
      rig.shell.commands.byId("files.read"),
      "a command the node no longer offers is still in the palette",
    ).toBeNull();
  });

  it("sends an adopted command back to the node it came from", async () => {
    const commands = createCommands({ storage: memoryStorage().env });
    const sent: Array<{ id: string; node?: string }> = [];
    commands.adopt([{ id: "files.read", title: "Read a file", risk: "read_only", node: "pocket" }], async (id, node) => {
      sent.push({ id, node });
    });

    await commands.dispatch("files.read");

    expect(sent, "an adopted command did not reach the bus").toEqual([
      { id: "files.read", node: "pocket" },
    ]);
  });
});

describe("a feature that registers commands and then goes away", () => {
  it("takes its own commands with it and leaves everything else", () => {
    const commands = createCommands({ storage: memoryStorage().env });
    commands.register([entry("app.open.files", "Open Files")]);
    const unregister = commands.register([
      entry("plugin.one", "Plugin one"),
      entry("plugin.two", "Plugin two"),
    ]);

    unregister();

    expect(commands.byId("plugin.one"), "unregister left the command behind").toBeNull();
    expect(commands.byId("plugin.two")).toBeNull();
    expect(commands.byId("app.open.files"), "unregister took someone else's command").toBeTruthy();
  });
});

describe("dispatching something that is not there", () => {
  it("says so, because a shortcut that silently does nothing is the worst kind", async () => {
    const commands = createCommands({ storage: memoryStorage().env });

    await commands.dispatch("app.open.nowhere");

    expect(commands.lastError.value, "a missing command was swallowed").toBeTruthy();
    expect(commands.lastError.value, "the error does not name the command").toContain(
      "app.open.nowhere",
    );
  });

  it("stops complaining once something runs", async () => {
    const commands = createCommands({ storage: memoryStorage().env });
    commands.register([entry("app.open.files", "Open Files")]);

    await commands.dispatch("app.open.nowhere");
    await commands.dispatch("app.open.files");

    expect(commands.lastError.value, "a stale error outlived the failure").toBeNull();
  });

  it("reports what a command that threw said, rather than looking like it worked", async () => {
    const commands = createCommands({ storage: memoryStorage().env });
    commands.register([
      entry("app.open.files", "Open Files", {
        run: () => {
          throw new Error("no such app");
        },
      }),
    ]);

    await commands.dispatch("app.open.files");

    expect(commands.lastError.value).toContain("no such app");
  });
});

describe("a chord as the operator typed it", () => {
  it("is spelled from their layout, not from where the key sits on a US board", () => {
    // Dvorak: the semicolon key reports code "KeyZ". A table keyed on the code
    // would make mod+; unreachable for them and bind mod+z by accident.
    const chord = chordOf(press({ key: ";", code: "KeyZ", ctrlKey: true }), false);

    expect(chord, "the chord followed the physical key instead of the layout").toBe("mod+;");
  });

  it("means Command on an Apple keyboard and Control on every other one", () => {
    expect(chordOf(press({ key: "k", metaKey: true }), true)).toBe("mod+k");
    expect(chordOf(press({ key: "k", ctrlKey: true }), false)).toBe("mod+k");
    // And the other one is not quietly treated as the same key. Asserted as
    // the exact spelling rather than as "not mod+k": a negation is satisfied
    // by every wrong answer as readily as by the right one, and two separate
    // ways of getting `mod` wrong hid behind it.
    expect(chordOf(press({ key: "k", metaKey: true }), false), "Win+K was not spelled as Meta").toBe(
      "meta+k",
    );
    expect(
      chordOf(press({ key: "k", ctrlKey: true }), true),
      "Ctrl+K on an Apple keyboard was not spelled as Ctrl",
    ).toBe("ctrl+k");
  });

  it("names the second modifier after the keyboard it is actually on", () => {
    // `mod` takes one of Command and Control, and the other stays reportable
    // so that mod+ctrl remains expressible. Name the leftover after the wrong
    // platform and the chord the event produces can never match the chord the
    // table stores: the key does nothing and the palette advertises it anyway.
    expect(
      chordOf(press({ key: "x", metaKey: true, ctrlKey: true }), true),
      "the second modifier on an Apple keyboard is Control",
    ).toBe("mod+ctrl+x");
    expect(
      chordOf(press({ key: "x", ctrlKey: true, metaKey: true }), false),
      "the second modifier on every other keyboard is Meta",
    ).toBe("mod+meta+x");
  });

  it("puts the modifiers in the one order the chord table is written in", () => {
    // The spelling *is* the lookup key, so Alt before Shift is not cosmetic.
    // Nothing shipped uses both today, which is why a swap here would surface
    // as the first operator to rebind onto such a chord finding a dead key.
    expect(
      chordOf(press({ key: "x", ctrlKey: true, altKey: true, shiftKey: true }), false),
      "modifier order is part of the chord spelling",
    ).toBe("mod+alt+shift+x");
  });

  it("reaches a command bound to a chord that uses both Alt and Shift", () => {
    // The round trip: the table is keyed by the table's spelling and looked up
    // by the event's, so this fails if the two ever disagree.
    const keyboard = createKeyboard({ apple: false, storage: memoryStorage().env });
    keyboard.bind([{ commandId: "x.windows.tile", chord: "mod+alt+shift+t" }]);

    expect(
      keyboard.resolve(press({ key: "t", ctrlKey: true, altKey: true, shiftKey: true })),
      "a chord using both Alt and Shift was unreachable",
    ).toBe("x.windows.tile");
  });

  it("reaches the command an Apple operator expects from Command, not Control", () => {
    const keyboard = createKeyboard({ apple: true, storage: memoryStorage().env });
    keyboard.bind([{ commandId: "shell.palette.open", chord: "mod+k" }]);

    expect(keyboard.resolve(press({ key: "k", metaKey: true }))).toBe("shell.palette.open");
    expect(
      keyboard.resolve(press({ key: "k", ctrlKey: true })),
      "Control fired a Command shortcut on a Mac",
    ).toBeNull();
  });
});

describe("rebinding a shortcut", () => {
  it("moves the command to the new chord and leaves the old one dead", () => {
    const keyboard = createKeyboard({ apple: false, storage: memoryStorage().env });
    keyboard.bind([{ commandId: "shell.palette.open", chord: "mod+k" }]);

    keyboard.rebind("shell.palette.open", "mod+p");

    expect(keyboard.resolve(press({ key: "p", ctrlKey: true }))).toBe("shell.palette.open");
    expect(
      keyboard.resolve(press({ key: "k", ctrlKey: true })),
      "the old chord still fires, so the command has two shortcuts",
    ).toBeNull();
    expect(keyboard.chordFor("shell.palette.open")).toBe("mod+p");
  });

  it("is still in force after the shell is rebuilt", () => {
    const storage = memoryStorage();
    const first = createKeyboard({ apple: false, storage: storage.env });
    first.bind(DEFAULT_SHORTCUTS);
    first.rebind("shell.switcher.open", "mod+alt+j");

    const rebuilt = createKeyboard({ apple: false, storage: storage.env });
    rebuilt.bind(DEFAULT_SHORTCUTS);

    expect(
      rebuilt.resolve(press({ key: "j", ctrlKey: true, altKey: true })),
      "a rebinding did not survive a reload",
    ).toBe("shell.switcher.open");
    expect(
      rebuilt.resolve(press({ key: "j", ctrlKey: true })),
      "the shipped chord came back alongside the operator's",
    ).toBeNull();
  });

  it("shows the operator the chord they chose, in their keyboard's symbols", () => {
    const apple = createKeyboard({ apple: true, storage: memoryStorage().env });
    apple.bind([{ commandId: "shell.palette.open", chord: "mod+k" }]);
    const pc = createKeyboard({ apple: false, storage: memoryStorage().env });
    pc.bind([{ commandId: "shell.palette.open", chord: "mod+k" }]);

    expect(apple.labelFor("shell.palette.open")).toBe("⌘K");
    expect(pc.labelFor("shell.palette.open")).toBe("Ctrl+K");
    expect(pc.labelFor("shell.nothing.bound")).toBeNull();
  });

  it("does not cost every other shortcut when the stored overrides are unreadable", () => {
    const storage = memoryStorage({ "deckjack.shortcuts": "}{ truncated" });

    const keyboard = createKeyboard({ apple: false, storage: storage.env });
    keyboard.bind(DEFAULT_SHORTCUTS);

    expect(
      keyboard.resolve(press({ key: "j", ctrlKey: true })),
      "corrupt overrides took the shipped shortcuts with them",
    ).toBe("shell.switcher.open");
  });
});

describe("the shell an operator actually gets", () => {
  it("has a command behind every shortcut it ships", () => {
    // The failure this is written for: a chord table and a registry that are
    // each fine on their own and were never introduced to each other, so a
    // shortcut resolves to an id nothing has registered and the key does
    // nothing. Every id is collected rather than asserted one at a time, so a
    // failure names the whole set instead of stopping at the first.
    const rig = harness();

    const dangling = DEFAULT_SHORTCUTS.filter(
      (s) => rig.shell.commands.byId(s.commandId) === null,
    ).map((s) => `${s.chord} -> ${s.commandId}`);

    expect(dangling, "these shortcuts are bound to commands nothing registered").toEqual([]);
  });

  it("opens the palette when the operator presses the chord that opens the palette", async () => {
    // RED, and left red: this documents a defect rather than a misreading.
    // `DEFAULT_SHORTCUTS` gives `shell.palette.open` both `mod+k` and
    // `mod+shift+p`, but the chord table keeps one chord per command, so
    // binding the shipped table evicts `mod+k` and Ctrl/Cmd+K does nothing.
    // One of the two is wrong — the alias in the table, or the eviction in
    // `install` — and until that is settled the universal palette chord is
    // dead in the shipped shell. Do not weaken this to match.
    const rig = harness();
    expect(rig.shell.ui.paletteOpen.value).toBe(false);

    const id = rig.shell.keyboard.resolve(press({ key: "k", ctrlKey: true }));
    expect(id, "Ctrl+K reached nothing").toBe("shell.palette.open");
    await rig.shell.commands.dispatch(id!);

    expect(rig.shell.ui.paletteOpen.value, "the palette did not open").toBe(true);
  });

  it("answers the palette's other shipped chord", () => {
    // The surviving half of the pair above: `mod+shift+p` is bound last and so
    // is the one the table keeps. Asserted separately so that fixing the alias
    // problem cannot quietly cost the chord that does work today.
    const rig = harness();

    expect(
      rig.shell.keyboard.resolve(press({ key: "p", ctrlKey: true, shiftKey: true })),
      "Ctrl+Shift+P reached nothing",
    ).toBe("shell.palette.open");
  });

  it("does nothing at all for a chord nobody bound", async () => {
    const rig = harness();

    const id = rig.shell.keyboard.resolve(press({ key: "q", ctrlKey: true, shiftKey: true }));

    expect(id, "an unbound chord resolved to a command").toBeNull();
    expect(rig.shell.ui.paletteOpen.value).toBe(false);
    expect(rig.shell.commands.lastError.value, "an unbound chord was dispatched anyway").toBeNull();
  });
});

describe("modifiers, and what counts as one", () => {
  it("does not treat Shift alone as a modifier", () => {
    // Found by mutation testing. `resolve()` decides a chord may fire inside a
    // text field when it "has a modifier", and asks `chord.includes("+")`.
    // A shift-only chord contains a "+", so holding Shift to type a capital
    // letter would dispatch a command instead. Latent today -- nothing in
    // DEFAULT_SHORTCUTS is shift-only -- and a trap for the first one that is.
    const keyboard = createKeyboard({ apple: false, storage: memoryStorage().env });
    keyboard.bind([{ commandId: "x.shifted", chord: "shift+n" }]);

    const typing = document.createElement("input");
    document.body.appendChild(typing);
    const event = new KeyboardEvent("keydown", { key: "N", shiftKey: true });
    Object.defineProperty(event, "target", { value: typing });

    expect(
      keyboard.resolve(event),
      "a capital letter typed into a field dispatched a command",
    ).toBeNull();
  });

  it("still lets a real modifier chord through while typing", () => {
    // The pair. Narrowing "has a modifier" must not take Ctrl+W with it.
    const keyboard = createKeyboard({ apple: false, storage: memoryStorage().env });
    keyboard.bind([{ commandId: "x.close", chord: "mod+w" }]);

    const typing = document.createElement("input");
    document.body.appendChild(typing);
    const event = new KeyboardEvent("keydown", { key: "w", ctrlKey: true });
    Object.defineProperty(event, "target", { value: typing });

    expect(keyboard.resolve(event)).toBe("x.close");
  });
});

describe("rebinding a shortcut", () => {
  it("keeps its permission to fire while typing", () => {
    // Found by mutation testing: `rebind` rebuilt the shortcut without
    // carrying `whileTyping`, so customizing a chord silently revoked the one
    // thing that made it usable in a text field.
    const keyboard = createKeyboard({ apple: false, storage: memoryStorage().env });
    keyboard.bind([{ commandId: "x.find", chord: "f3", whileTyping: true }]);

    keyboard.rebind("x.find", "f4");

    const typing = document.createElement("input");
    document.body.appendChild(typing);
    const event = new KeyboardEvent("keydown", { key: "F4" });
    Object.defineProperty(event, "target", { value: typing });
    expect(
      keyboard.resolve(event),
      "rebinding took away the shortcut's permission to work while typing",
    ).toBe("x.find");
  });

  it("takes the chord off whoever held it before", () => {
    // Found by mutation testing. Moving a command onto a chord another command
    // already owns left the table with two claims on it: the loser still
    // reported the chord through chordFor/labelFor, and rebinding the loser
    // afterwards unbound the winner's key while still advertising it.
    const keyboard = createKeyboard({ apple: false, storage: memoryStorage().env });
    keyboard.bind([
      { commandId: "x.first", chord: "mod+1" },
      { commandId: "x.second", chord: "mod+2" },
    ]);

    keyboard.rebind("x.second", "mod+1");

    const event = new KeyboardEvent("keydown", { key: "1", ctrlKey: true });
    expect(keyboard.resolve(event), "the chord did not move").toBe("x.second");
    expect(
      keyboard.chordFor("x.first"),
      "the command that lost the chord still claims it",
    ).not.toBe("mod+1");
  });
});

describe("a keystroke the operator meant as text", () => {
  it("belongs to the field they typed it into, not to a bare-key shortcut", () => {
    const keyboard = createKeyboard({ apple: false, storage: memoryStorage().env });
    keyboard.bind([
      { commandId: "x.window.new", chord: "n" },
      { commandId: "x.cancel", chord: "escape" },
    ]);
    const input = textField();
    const area = textField("textarea");

    expect(
      keyboard.resolve(pressInto(input, { key: "n" })),
      "typing an n in a field opened a new window",
    ).toBeNull();
    expect(
      keyboard.resolve(pressInto(area, { key: "n" })),
      "a textarea is a place to type too",
    ).toBeNull();
    expect(
      keyboard.resolve(pressInto(input, { key: "Escape" })),
      "a bare Escape fired from inside a text field",
    ).toBeNull();
    // Nothing is wrong with the chord itself -- the same key still works when
    // the operator is not typing, which is the whole point of the guard.
    expect(keyboard.resolve(press({ key: "n" })), "the bare chord stopped working entirely").toBe(
      "x.window.new",
    );
  });

  it("does not take a real chord, or one that asked for the exception, with it", () => {
    // The pair to the test above: the guard exists to protect text entry, and
    // it has done too much if Ctrl+K no longer opens the palette from the
    // palette's own search box, or if whileTyping stops meaning anything.
    const keyboard = createKeyboard({ apple: false, storage: memoryStorage().env });
    keyboard.bind([
      { commandId: "x.palette.open", chord: "mod+k" },
      { commandId: "x.find", chord: "f", whileTyping: true },
    ]);
    const input = textField();

    expect(
      keyboard.resolve(pressInto(input, { key: "k", ctrlKey: true })),
      "Ctrl+K must open the palette from a text field",
    ).toBe("x.palette.open");
    expect(
      keyboard.resolve(pressInto(input, { key: "f" })),
      "whileTyping was ignored",
    ).toBe("x.find");
  });

  it("is left to the IME while a composition is still open", () => {
    // Composing Japanese, Chinese or Korean, candidate selection uses keys
    // that are also chords. Stealing them makes text entry impossible for an
    // operator an ASCII-only test run never models.
    const keyboard = createKeyboard({ apple: false, storage: memoryStorage().env });
    keyboard.bind([{ commandId: "x.palette.open", chord: "mod+k" }]);

    expect(
      keyboard.resolve(press({ key: "k", ctrlKey: true, isComposing: true })),
      "a shortcut fired in the middle of an IME composition",
    ).toBeNull();
  });
});

describe("asking the browser for the keys it normally keeps for itself", () => {
  it("asks for the chords the shell needs back", async () => {
    const asked: Array<string[] | undefined> = [];
    const keyboard = createKeyboard({
      apple: false,
      storage: memoryStorage().env,
      keyboard: { lock: async (keys) => void asked.push(keys), unlock: () => {} },
    });

    expect(await keyboard.requestLock()).toBe("locked");
    // Spelled out rather than compared against LOCKED_KEYS: a test that reads
    // the same list the code does would agree with an empty one, and an empty
    // request is a lock the status claims and the browser never granted.
    expect(asked[0], "the lock request did not name the keys the shell needs").toEqual([
      "KeyW",
      "KeyT",
      "Tab",
      "Escape",
    ]);
  });

  it("tells the operator when the browser refused", async () => {
    // Chromium refuses outside an installed window or fullscreen. Reporting
    // that as success is worse than not asking: the operator presses Ctrl+W
    // expecting a Deckjack window to close and closes the whole shell.
    const keyboard = createKeyboard({
      apple: false,
      storage: memoryStorage().env,
      keyboard: { lock: () => Promise.reject(new Error("not fullscreen")), unlock: () => {} },
    });

    expect(await keyboard.requestLock(), "a refused lock was reported as locked").toBe("refused");
    expect(keyboard.lockStatus.value).toBe("refused");
  });

  it("says so in a browser that has no keyboard lock at all", async () => {
    const keyboard = createKeyboard({ apple: false, storage: memoryStorage().env });

    expect(await keyboard.requestLock()).toBe("unsupported");
    expect(keyboard.lockStatus.value).toBe("unsupported");
  });

  it("hands the keys back when the lock is released", async () => {
    let unlocked = 0;
    const keyboard = createKeyboard({
      apple: false,
      storage: memoryStorage().env,
      keyboard: {
        lock: async () => {},
        unlock: () => {
          unlocked += 1;
        },
      },
    });

    await keyboard.requestLock();
    keyboard.releaseLock();

    expect(unlocked, "the browser's keys were still captured after releasing the lock").toBe(1);
    expect(keyboard.lockStatus.value).toBe("idle");
  });
});

describe("dispatching a command that says it cannot run right now", () => {
  it("does not run it, whichever way the operator asked", async () => {
    // The palette hides an unavailable command, so the hole only shows on the
    // other paths: a shortcut, the CLI, an agent. Without this check Ctrl+W on
    // an empty workspace closes a window that is not there.
    let ran = 0;
    const commands = createCommands({ storage: memoryStorage().env });
    commands.register([
      entry("shell.window.close", "Close window", {
        available: () => false,
        run: () => {
          ran += 1;
        },
      }),
    ]);

    await commands.dispatch("shell.window.close");

    expect(ran, "a command that said it cannot run was run anyway").toBe(0);
    // And it is not a wiring bug: nothing is broken, the command simply has
    // nothing to act on, so the shell must not report an error for it.
    expect(commands.lastError.value, "an unavailable command looked like a wiring bug").toBeNull();
  });
});

describe("how much the palette remembers", () => {
  it("keeps the whole run of recent commands, not just the last one", async () => {
    const commands = createCommands({ storage: memoryStorage().env });
    commands.register([
      entry("app.open.files", "Open Files"),
      entry("app.open.notes", "Open Notes"),
      entry("app.open.mail", "Open Mail"),
    ]);

    await commands.dispatch("app.open.files");
    await commands.dispatch("app.open.notes");
    await commands.dispatch("app.open.mail");

    // The empty palette is the view the operator opens most, and its promise
    // is a history in most-recent-first order -- not one remembered command
    // followed by registration order.
    expect(titles(commands.search("")), "the palette remembered only the last command").toEqual([
      "app.open.mail",
      "app.open.notes",
      "app.open.files",
    ]);
  });

  it("offers a dozen rows when the caller does not say how many", () => {
    // The palette relies on the default, so a default of "all of them" makes
    // an unscrollable wall on a node with many adapter commands -- and on the
    // Pocket's screen the first row is the only one the operator can reach.
    const commands = createCommands({ storage: memoryStorage().env });
    commands.register(
      Array.from({ length: 20 }, (_, i) => entry(`app.open.${i}`, `Open thing ${i}`)),
    );

    expect(commands.search("open").length, "the default page size for a query").toBe(12);
    expect(commands.search("").length, "the default page size for an empty palette").toBe(12);
  });
});

describe("what the operator is told a command will do", () => {
  it("never describes a command as gentler than it is", () => {
    // This label is what the confirmation dialog shows, so a destructive
    // command labeled "reads" is the operator confirming something they were
    // told was harmless. Every level is asserted at once, so collapsing them
    // all onto one answer cannot pass.
    const risks: Risk[] = ["read_only", "write", "cross_project", "outbound", "destructive"];

    expect(risks.map(riskLabel)).toEqual([
      "reads",
      "writes",
      "touches another project",
      "sends something out",
      "destructive",
    ]);
  });
});

describe("a command that needs to be told what to act on", () => {
  it("is given its parameters, so an agent and the palette reach it the same way", async () => {
    // Principle 3: every action has an id and typed parameters, and the UI,
    // the keyboard, the palette, the CLI and agents all dispatch the same
    // command. `run()` took none, so a command like Quick Look -- which is
    // meaningless without a path -- could only have been driven by reaching
    // into a panel's selection, which is the one caller an agent does not
    // have.
    const store = createCommands({ storage: memoryStorage().env });
    const seen: Array<Record<string, unknown> | undefined> = [];
    store.register([
      {
        id: "x.peek",
        title: "Peek",
        risk: "read_only",
        group: "Test",
        run: (params) => void seen.push(params),
      },
    ]);

    await store.dispatch("x.peek", { path: "/Photos/a.jpg" });

    expect(seen).toEqual([{ path: "/Photos/a.jpg" }]);
  });

  it("still runs the commands that take none", async () => {
    const store = createCommands({ storage: memoryStorage().env });
    let ran = 0;
    store.register([
      { id: "x.plain", title: "Plain", risk: "read_only", group: "Test", run: () => void ran++ },
    ]);

    await store.dispatch("x.plain");

    expect(ran).toBe(1);
  });
});
