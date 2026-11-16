// @vitest-environment happy-dom
import { describe, expect, it } from "vitest";

import { createCommands, type CommandEntry, type CommandsEnv } from "./commands.ts";
import { chordOf, createKeyboard, DEFAULT_SHORTCUTS } from "../shell/keyboard.ts";
import { harness } from "../testing/shell-harness.ts";
import type { CommandSpec } from "../protocol.ts";

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
    // And the other one is not quietly treated as the same key.
    expect(chordOf(press({ key: "k", metaKey: true }), false), "Win+K was read as Ctrl+K").not.toBe(
      "mod+k",
    );
    expect(chordOf(press({ key: "k", ctrlKey: true }), true), "Ctrl+K was read as Cmd+K").not.toBe(
      "mod+k",
    );
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
