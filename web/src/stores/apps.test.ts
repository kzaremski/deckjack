import { describe, expect, it } from "vitest";

import { builtinApps, createAppTable, parseInstalled, tagFrom } from "./apps.ts";

/**
 * The app table: the built-in entries merged with what `system.apps` reports.
 *
 * The promise is that what is installed on the node is what the shell
 * offers, in the manifest's own words. It used to be a closed list compiled
 * into the bundle, so an app installed after a build could not be opened from
 * anywhere.
 */

function tableWith(apps: unknown[]) {
  const table = createAppTable();
  table.installed.value = parseInstalled({ apps });
  return table;
}

const photo = {
  name: "photo",
  title: "Photos",
  version: "2.0.0",
  lifecycle: "on-demand",
  status: "live",
  tag: "Ph",
  summary: "Edit photographs",
  min_width: 360,
  default_width: 900,
  default_height: 600,
  compact: true,
  wide: false,
};

describe("an installed app the shell was not built with", () => {
  it("joins the dock, ahead of the Assistant", () => {
    const ids = tableWith([photo]).dock.value.map((a) => a.id);
    expect(ids).toContain("photo");
    expect(ids.at(-1), "the Assistant is one app among many and sits last").toBe("assistant");
    expect(ids.indexOf("photo")).toBeLessThan(ids.indexOf("assistant"));
  });

  it("is described by its manifest", () => {
    const contract = tableWith([photo]).contract("photo");
    expect(contract.title).toBe("Photos");
    expect(contract.tag).toBe("Ph");
    expect(contract.summary).toBe("Edit photographs");
    expect(contract.minSize.w).toBe(360);
    expect(contract.defaultSize).toEqual({ w: 900, h: 600 });
    expect(contract.wide, "the manifest said it has no wide layout").toBe(false);
    expect(contract.compact).toBe(true);
  });

  it("gets a readable tag from its title when the manifest gave none", () => {
    const { tag, ...rest } = photo;
    void tag;
    expect(tableWith([rest]).contract("photo").tag).toBe("Ph");
    expect(tagFrom("x")).toBe("X");
    expect(tagFrom("  3d viewer")).toBe("3d");
  });

  it("never opens narrower than it says it can work at", () => {
    const contract = tableWith([{ ...photo, default_width: undefined, min_width: 1000 }]).contract(
      "photo",
    );
    expect(contract.defaultSize.w).toBeGreaterThanOrEqual(1000);
  });

  it("supports both layouts unless the manifest takes one away", () => {
    const { compact, wide, ...silent } = photo;
    void compact;
    void wide;
    const contract = tableWith([silent]).contract("photo");
    expect(contract.compact && contract.wide).toBe(true);
  });
});

describe("an installed app the shell already has an entry for", () => {
  it("is described by its manifest wherever the manifest says something", () => {
    const table = tableWith([
      { name: "notes", title: "Notebook", version: "1", status: "live", tag: "Nb", default_width: 700 },
    ]);
    const notes = table.contract("notes");
    expect(notes.title).toBe("Notebook");
    expect(notes.tag).toBe("Nb");
    expect(notes.defaultSize.w).toBe(700);
    // ...and the built-in entry fills what it did not say.
    const builtin = builtinApps.find((a) => a.id === "notes")!;
    expect(notes.defaultSize.h).toBe(builtin.defaultSize.h);
  });

  it("keeps its dock slot rather than moving to the end", () => {
    const table = tableWith([{ name: "notes", title: "Notes", version: "1", status: "live" }]);
    expect(table.dock.value.map((a) => a.id)).toEqual(builtinApps.map((a) => a.id));
  });

  it("keeps the Inbox badge, which is the core's and not the manifest's", () => {
    const table = tableWith([{ name: "assistant", title: "Assistant", version: "1", status: "live" }]);
    expect(table.contract("assistant").badge).toBe("inbox");
    expect(table.dock.value.at(-1)!.id).toBe("assistant");
  });
});

describe("what the core reports", () => {
  it("drops entries it cannot use rather than guessing", () => {
    const parsed = parseInstalled({ apps: [null, 3, { title: "nameless" }, photo] });
    expect(parsed.map((a) => a.name)).toEqual(["photo"]);
    expect(parseInstalled(undefined)).toEqual([]);
    expect(parseInstalled({ apps: "nope" })).toEqual([]);
  });

  it("an empty install leaves exactly the built-in dock", () => {
    expect(tableWith([]).dock.value.map((a) => a.id)).toEqual(builtinApps.map((a) => a.id));
  });
});

describe("a window whose app nothing knows", () => {
  it("is itself, not some other app", () => {
    // The old fallback gave every unknown app the Files id, so a window for
    // an uninstalled app was treated as Files everywhere that read the id.
    const contract = createAppTable().contract("gone");
    expect(contract.id).toBe("gone");
    expect(contract.title).toBe("gone");
    expect(createAppTable().isKnown("gone")).toBe(false);
  });
});
