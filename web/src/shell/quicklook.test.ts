import { describe, expect, it } from "vitest";

import {
  contentUrl,
  createQuickLook,
  formatBytes,
  formatDuration,
  openAppVia,
  resolveRelative,
  viewerFor,
  type QuickLookEnv,
} from "./quicklook.ts";

/**
 * What a preview owes the operator.
 *
 * Quick Look's whole promise is "look at this without launching the thing that
 * owns it", so the interesting behavior is not rendering — the browser does
 * that — but *judgement*: which files it can honestly show, which it must
 * admit it cannot, and which it must refuse to execute. A viewer chosen wrongly
 * is a blank frame with no explanation, which is worse than saying no.
 */

/** What the hand-off saw: every app it was asked to open, with the path. */
type Opened = Array<{ app: string; path: string }>;

function env(options: { online?: boolean; apps?: string[]; opened?: Opened } = {}): QuickLookEnv {
  const known = new Set(options.apps ?? []);
  return {
    online: () => options.online ?? true,
    apps: {
      isKnown: (id) => known.has(id),
      contract: (id) => ({ title: id === "scribe" ? "Editor" : id }),
    },
    openApp: (app, path) => {
      options.opened?.push({ app, path });
    },
  };
}

describe("choosing a viewer", () => {
  it("shows the things a browser can already decode", () => {
    expect(viewerFor("holiday.jpg").kind).toBe("image");
    expect(viewerFor("diagram.png").kind).toBe("image");
    expect(viewerFor("loop.webp").kind).toBe("image");
    expect(viewerFor("clip.mp4").kind).toBe("video");
    expect(viewerFor("song.mp3").kind).toBe("audio");
    expect(viewerFor("report.pdf").kind).toBe("pdf");
    expect(viewerFor("notes.txt").kind).toBe("text");
    expect(viewerFor("main.rs").kind).toBe("text");
    expect(viewerFor("Makefile").kind).toBe("text");
  });

  it("admits what it cannot decode instead of showing a broken frame", () => {
    // A camera RAW is the case that matters: no browser decodes CR2, NEF, ARW
    // or DNG, so the only honest answer until the node can render a thumbnail
    // is to say so. Guessing `image` here would put a broken <img> in front of
    // the operator with no explanation, which is the exact failure this test
    // exists to prevent.
    for (const name of ["DSC_0001.NEF", "IMG_1234.CR2", "a.arw", "b.dng"]) {
      const v = viewerFor(name);
      expect(v.kind, `${name} was promised a viewer that cannot render it`).toBe("none");
      expect(v.why, `${name} gave no reason`).toBeTruthy();
    }
  });

  it("says so for a format nobody here can open", () => {
    const v = viewerFor("archive.7z");
    expect(v.kind).toBe("none");
    expect(v.why).toBeTruthy();
  });

  it("renders Markdown as a document, not as its source", () => {
    expect(viewerFor("README.md").kind).toBe("markdown");
    expect(viewerFor("notes.markdown").kind).toBe("markdown");
    expect(viewerFor("x.bin", "text/markdown").kind).toBe("markdown");
  });

  it("lists an archive the node can read, and names the ones it cannot", () => {
    for (const name of ["src.zip", "release.tar", "release.tar.gz", "release.tgz", "server.log.gz"]) {
      expect(viewerFor(name).kind, `${name} is listable`).toBe("archive");
    }
    // The refusal names the format, so the operator knows it is the format
    // and not the file that is the problem.
    for (const [name, format] of [
      ["bundle.7z", /7-?zip|7z/i],
      ["old.rar", /rar/i],
      ["release.tar.xz", /xz/i],
    ] as const) {
      const v = viewerFor(name);
      expect(v.kind, `${name} was promised a listing`).toBe("none");
      expect(v.why, `${name}'s refusal does not name it`).toMatch(format);
    }
  });

  it("does not decide from a name a file merely happens to have", () => {
    // The server states the type; the extension is a hint for picking a
    // viewer before the bytes arrive. When the two disagree, the server wins,
    // because it is the thing that set the Content-Type the browser obeys.
    expect(viewerFor("mystery.bin", "image/png").kind).toBe("image");
    expect(viewerFor("photo.png", "application/pdf").kind).toBe("pdf");
  });
});

describe("a file that can run code", () => {
  it("is previewed in something sandboxed, or not at all", () => {
    // An HTML file or an SVG in the working directory is attacker-authored as
    // far as the shell is concerned: it could have arrived by download, email,
    // sync or an agent. Previewed on the shell's own origin it would run with
    // the operator's session. The content route sends
    // `Content-Security-Policy: sandbox`, and this is the other half --
    // whatever renders it must be a frame that is itself sandboxed.
    for (const name of ["page.html", "logo.svg", "feed.xml"]) {
      const v = viewerFor(name);
      expect(v.sandboxed, `${name} would have rendered unsandboxed`).toBe(true);
    }
  });

  it("does not sandbox what cannot execute, because that would cost features", () => {
    // A <video> in a sandboxed frame loses its controls for nothing: a
    // container cannot execute. Sandboxing everything would be the easy
    // answer and the wrong one.
    expect(viewerFor("clip.mp4").sandboxed).toBe(false);
    expect(viewerFor("holiday.jpg").sandboxed).toBe(false);
  });
});

describe("the address the browser fetches", () => {
  it("asks the node for the file by its own path", () => {
    expect(contentUrl("/Photos/holiday.jpg")).toBe("/content?path=%2FPhotos%2Fholiday.jpg");
  });

  it("survives a name that would otherwise end the query string", () => {
    // `&`, `?`, `#` and a space in a filename are ordinary. Unescaped, the
    // first one truncates the path and the node is asked for a different file
    // than the operator clicked -- or for nothing at all.
    const url = contentUrl("/a&b/c?d#e/my file.txt");
    expect(url).not.toMatch(/[ #]/);
    expect(url.match(/[?]/g)?.length, "a second ? would split the query").toBe(1);
    expect(new URLSearchParams(url.split("?")[1]!).get("path")).toBe("/a&b/c?d#e/my file.txt");
  });

  it("offers a download as a different request, not a different viewer", () => {
    const url = contentUrl("/report.pdf", { download: true });
    expect(new URLSearchParams(url.split("?")[1]!).get("download")).toBe("1");
  });
});

describe("a Markdown file's own images", () => {
  it("are found next to the file, the way the file's author meant", () => {
    expect(resolveRelative("/docs/README.md", "img/shot.png")).toBe("/docs/img/shot.png");
    expect(resolveRelative("/docs/README.md", "./shot.png")).toBe("/docs/shot.png");
    expect(resolveRelative("/docs/guide/intro.md", "../assets/a.png")).toBe("/docs/assets/a.png");
    expect(resolveRelative("/docs/README.md", "/logo.png")).toBe("/logo.png");
    expect(resolveRelative("/README.md", "my%20shot.png?raw=1#top")).toBe("/my shot.png");
  });

  it("are not looked for above the working directory or on another host", () => {
    expect(resolveRelative("/README.md", "../../etc/passwd")).toBeNull();
    expect(resolveRelative("/README.md", "https://example.com/a.png")).toBeNull();
    expect(resolveRelative("/README.md", "//example.com/a.png")).toBeNull();
    expect(resolveRelative("/README.md", "data:image/png;base64,AAAA")).toBeNull();
  });
});

describe("numbers worth reading", () => {
  it("gives sizes in the units a file manager uses", () => {
    expect(formatBytes(1)).toBe("1 byte");
    expect(formatBytes(999)).toBe("999 bytes");
    expect(formatBytes(1_500)).toBe("1.5 KB");
    expect(formatBytes(2_400_000_000)).toBe("2.4 GB");
    expect(formatBytes(524_288)).toBe("524 KB");
  });

  it("gives durations as a clock", () => {
    expect(formatDuration(65)).toBe("1:05");
    expect(formatDuration(3723)).toBe("1:02:03");
    expect(formatDuration(Number.NaN)).toBe("");
  });
});

describe("moving through a folder", () => {
  const folder = ["/a.jpg", "/b.png", "/c.mp4"];

  it("opens what was asked for", () => {
    const ql = createQuickLook(env());
    ql.open("/b.png", folder);
    expect(ql.visible.value).toBe(true);
    expect(ql.current.value).toBe("/b.png");
  });

  it("steps to the neighbours, because that is how a folder is reviewed", () => {
    const ql = createQuickLook(env());
    ql.open("/a.jpg", folder);

    ql.next();
    expect(ql.current.value).toBe("/b.png");
    ql.next();
    expect(ql.current.value).toBe("/c.mp4");
    ql.previous();
    expect(ql.current.value).toBe("/b.png");
  });

  it("stops at the ends rather than wrapping around", () => {
    // Wrapping makes "next" at the end look like nothing happened, and hides
    // from the operator that they have seen everything.
    const ql = createQuickLook(env());
    ql.open("/c.mp4", folder);

    ql.next();
    expect(ql.current.value, "stepping past the last file moved somewhere").toBe("/c.mp4");

    ql.open("/a.jpg", folder);
    ql.previous();
    expect(ql.current.value).toBe("/a.jpg");
  });

  it("can be opened on a file with no folder around it", () => {
    const ql = createQuickLook(env());
    ql.open("/lonely.txt");
    expect(ql.current.value).toBe("/lonely.txt");
    ql.next();
    expect(ql.current.value).toBe("/lonely.txt");
  });

  it("is closed, and forgets what it was showing", () => {
    // A stale path left behind would be served the next time the overlay
    // opened, before the new one loaded: the previous file flashing up inside
    // the new one's frame.
    const ql = createQuickLook(env());
    ql.open("/a.jpg", folder);
    ql.close();

    expect(ql.visible.value).toBe(false);
    expect(ql.current.value).toBeNull();
  });
});

describe("with no node to fetch from", () => {
  it("says this device is offline instead of showing an empty frame", () => {
    // The content route is on the node. Offline, the <img> would simply fail
    // and the operator would be looking at a blank overlay wondering whether
    // the file is corrupt.
    const ql = createQuickLook(env({ online: false }));
    ql.open("/a.jpg");

    expect(ql.visible.value, "the overlay should still open to explain itself").toBe(true);
    expect(ql.unavailable.value).toMatch(/offline/i);
  });

  it("does not claim anything is wrong when the node is there", () => {
    const ql = createQuickLook(env());
    ql.open("/a.jpg");
    expect(ql.unavailable.value).toBeNull();
  });
});

describe("opening the file fully", () => {
  it("hands a text file to the editor, with its path, and gets out of the way", () => {
    const opened: Opened = [];
    const ql = createQuickLook(env({ apps: ["scribe"], opened }));
    ql.open("/src/main.rs", ["/src/lib.rs", "/src/main.rs"]);

    expect(ql.handoff.value?.app).toBe("scribe");
    expect(ql.handoff.value?.title).toBe("Editor");
    expect(ql.openFully()).toBe(true);

    expect(opened).toEqual([{ app: "scribe", path: "/src/main.rs" }]);
    expect(ql.visible.value, "the preview stayed over the editor it opened").toBe(false);
  });

  it("follows the file the operator has stepped to, not the one they opened", () => {
    const opened: Opened = [];
    const ql = createQuickLook(env({ apps: ["scribe"], opened }));
    ql.open("/a.md", ["/a.md", "/b.md"]);
    ql.next();
    ql.openFully();
    expect(opened).toEqual([{ app: "scribe", path: "/b.md" }]);
  });

  it("offers nothing when no editor is installed, rather than a button that fails", () => {
    const opened: Opened = [];
    const ql = createQuickLook(env({ apps: ["filer", "notes"], opened }));
    ql.open("/README.md");

    expect(ql.handoff.value).toBeNull();
    expect(ql.openFully()).toBe(false);
    expect(opened).toEqual([]);
    expect(ql.visible.value).toBe(true);
  });

  it("does not send a photograph to a text editor", () => {
    const opened: Opened = [];
    const ql = createQuickLook(env({ apps: ["scribe"], opened }));
    ql.open("/holiday.jpg");
    expect(ql.handoff.value).toBeNull();
    expect(ql.openFully()).toBe(false);
    expect(opened).toEqual([]);
  });

  it("opens the app on the file through the window store's open-app entry", async () => {
    const calls: unknown[][] = [];
    const open = openAppVia({
      open: async (app) => void calls.push(["open", app]),
      openApp: async (app, options) => void calls.push(["openApp", app, options]),
    });
    await open("scribe", "/src/main.rs");
    expect(calls).toEqual([["openApp", "scribe", { path: "/src/main.rs" }]]);
  });

  it("still opens the app when the window store cannot carry a file yet", async () => {
    const calls: unknown[][] = [];
    const open = openAppVia({ open: async (app) => void calls.push(["open", app]) });
    await open("scribe", "/src/main.rs");
    expect(calls).toEqual([["open", "scribe"]]);
  });

  it("offers the source of an HTML page to the editor too", () => {
    // The preview of a web page is sandboxed and inert; editing it is what
    // "open fully" means for a file someone is writing.
    const ql = createQuickLook(env({ apps: ["scribe"] }));
    ql.open("/site/index.html");
    expect(ql.handoff.value?.app).toBe("scribe");
  });
});
