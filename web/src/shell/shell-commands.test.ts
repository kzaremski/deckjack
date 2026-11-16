// @vitest-environment happy-dom
import { describe, expect, it } from "vitest";

import { shellCommands } from "./shell-commands.ts";
import { harness } from "../testing/shell-harness.ts";

/**
 * What the shell's own command list owes the operator.
 *
 * The one promise asserted here is the one that was missing: every route for
 * enrolling a passkey has been mounted and working for as long as the passkey
 * module has existed, and nothing in the product called any of them. The only
 * way to register a credential was to paste a script into a devtools console,
 * which rules out every phone. A command is the cheapest honest fix -- it puts
 * enrollment in the palette, where every other thing the shell can do already
 * lives, rather than inventing a settings screen to hold one link.
 */

function commandsWith() {
  const rig = harness();
  const visited: string[] = [];
  const entries = shellCommands({
    windows: rig.shell.windows,
    ui: rig.shell.ui,
    bus: rig.shell.bus,
    theme: rig.shell.theme,
    viewport: rig.shell.viewport,
    keyboard: rig.shell.keyboard,
    quicklook: rig.shell.quicklook,
    navigate: (url) => void visited.push(url),
  });
  return { rig, entries, visited };
}

describe("setting up a passkey", () => {
  it("is something the shell offers, not something to paste into a console", () => {
    const { entries } = commandsWith();
    const found = entries.find((e) => e.id === "shell.passkey.enroll");
    expect(found, "the shell has no command for enrolling a passkey").toBeTruthy();
  });

  it("takes the operator to the enrollment page", () => {
    const { entries, visited } = commandsWith();
    const enroll = entries.find((e) => e.id === "shell.passkey.enroll")!;

    enroll.run();

    expect(visited, "the command went nowhere").toEqual(["/passkey/enroll"]);
  });

  it("is findable by the word an operator would actually search for", () => {
    // A command nobody can find is not in the product. "passkey" is the word
    // on the page it leads to and the word in every OS prompt it triggers.
    const { rig, entries } = commandsWith();
    rig.shell.commands.register(entries);

    const hits = rig.shell.commands.search("passkey", 10);

    expect(
      hits.some((h) => h.entry.id === "shell.passkey.enroll"),
      "searching for a passkey does not surface the way to set one up",
    ).toBe(true);
  });

  it("does not pretend to be a change to the node", () => {
    // It navigates this browser. Marking it a write would send it through a
    // confirmation it has no business asking for, and would let an agent
    // believe it had changed something on the node.
    const { entries } = commandsWith();
    const enroll = entries.find((e) => e.id === "shell.passkey.enroll")!;
    expect(enroll.risk).toBe("read_only");
  });
});

describe("Quick Look from the palette", () => {
  it("previews the file it was handed", () => {
    const rig = harness();
    const entries = shellCommands({
      windows: rig.shell.windows,
      ui: rig.shell.ui,
      bus: rig.shell.bus,
      theme: rig.shell.theme,
      viewport: rig.shell.viewport,
      keyboard: rig.shell.keyboard,
      quicklook: rig.shell.quicklook,
      navigate: () => {},
    });
    const peek = entries.find((e) => e.id === "shell.quicklook.open")!;

    peek.run({ path: "/Photos/holiday.jpg" });

    expect(rig.shell.quicklook.visible.value).toBe(true);
    expect(rig.shell.quicklook.current.value).toBe("/Photos/holiday.jpg");
  });

  it("says what is missing rather than opening an empty preview", () => {
    // Reachable from an agent and the CLI, so "called with nothing" is a real
    // case and not a hypothetical.
    const rig = harness();
    const entries = shellCommands({
      windows: rig.shell.windows,
      ui: rig.shell.ui,
      bus: rig.shell.bus,
      theme: rig.shell.theme,
      viewport: rig.shell.viewport,
      keyboard: rig.shell.keyboard,
      quicklook: rig.shell.quicklook,
      navigate: () => {},
    });
    const peek = entries.find((e) => e.id === "shell.quicklook.open")!;

    peek.run();

    expect(rig.shell.quicklook.visible.value, "an empty preview opened").toBe(false);
    expect(rig.shell.ui.notices.value.some((n) => n.kind === "error")).toBe(true);
  });
});
