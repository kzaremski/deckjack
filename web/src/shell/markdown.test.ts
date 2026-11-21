// @vitest-environment happy-dom
import { describe, expect, it } from "vitest";

import { renderMarkdown } from "./markdown.ts";

/**
 * Markdown, for Quick Look.
 *
 * Two promises, and the second is the one that matters. It renders the
 * Markdown people actually write -- headings, emphasis, lists, code, links,
 * tables -- and it never produces anything that can run. A README in the
 * working directory is attacker-authored as far as the shell is concerned,
 * and this output is put into the shell's own document.
 *
 * The safety tests parse the output with a real DOM parser and inspect what
 * came out, rather than grepping strings: an escaping bug that produced a
 * live `<img onerror>` would pass a test that only looked for `<script`.
 */

function dom(html: string): HTMLElement {
  const host = document.createElement("div");
  host.innerHTML = html;
  return host;
}

const noImages = { resolveImage: () => null };

describe("what it renders", () => {
  it("renders headings, paragraphs and emphasis", () => {
    const out = dom(renderMarkdown("# Title\n\nSome *soft* and **strong** `code`.", noImages));
    expect(out.querySelector("h1")?.textContent).toBe("Title");
    expect(out.querySelector("p em")?.textContent).toBe("soft");
    expect(out.querySelector("p strong")?.textContent).toBe("strong");
    expect(out.querySelector("p code")?.textContent).toBe("code");
  });

  it("renders a fenced code block verbatim, markup and all", () => {
    const src = "```html\n<b>not bold</b>\n  indented\n```";
    const out = dom(renderMarkdown(src, noImages));
    const code = out.querySelector("pre code");
    expect(code?.textContent).toBe("<b>not bold</b>\n  indented");
    expect(out.querySelector("b"), "code in a fence was rendered as markup").toBeNull();
  });

  it("renders ordered, unordered and nested lists", () => {
    const out = dom(renderMarkdown("- one\n- two\n  - two-a\n\n1. first\n2. second", noImages));
    expect([...out.querySelectorAll("ul > li")].map((li) => li.firstChild?.textContent?.trim())).toEqual(
      ["one", "two", "two-a"],
    );
    expect(out.querySelector("ul ul li")?.textContent).toBe("two-a");
    expect([...out.querySelectorAll("ol > li")].map((li) => li.textContent)).toEqual([
      "first",
      "second",
    ]);
  });

  it("renders block quotes, rules and tables", () => {
    const out = dom(
      renderMarkdown("> quoted\n\n---\n\n| a | b |\n|---|:-:|\n| 1 | 2 |", noImages),
    );
    expect(out.querySelector("blockquote")?.textContent?.trim()).toBe("quoted");
    expect(out.querySelector("hr")).not.toBeNull();
    expect([...out.querySelectorAll("th")].map((c) => c.textContent)).toEqual(["a", "b"]);
    expect([...out.querySelectorAll("td")].map((c) => c.textContent)).toEqual(["1", "2"]);
  });

  it("renders links that leave the shell in a new tab without an opener", () => {
    const out = dom(renderMarkdown("See [the docs](https://example.com/a?b=1&c=2).", noImages));
    const a = out.querySelector("a")!;
    expect(a.getAttribute("href")).toBe("https://example.com/a?b=1&c=2");
    expect(a.getAttribute("target")).toBe("_blank");
    expect(a.getAttribute("rel")).toContain("noopener");
    expect(a.getAttribute("rel")).toContain("noreferrer");
  });

  it("shows a relative image through the address it is given, and a remote one not at all", () => {
    // A remote image in a README is a tracking pixel that fires the moment
    // the operator peeks at a file. A relative one is the project's own
    // screenshot, on this node.
    const out = dom(
      renderMarkdown("![shot](img/a.png) ![pixel](https://tracker.example/p.gif)", {
        resolveImage: (src) => (src.startsWith("img/") ? `/content?path=%2Fdocs%2F${src}` : null),
      }),
    );
    const imgs = [...out.querySelectorAll("img")];
    expect(imgs.map((i) => i.getAttribute("src"))).toEqual(["/content?path=%2Fdocs%2Fimg/a.png"]);
    expect(out.textContent, "the remote image vanished without a word").toContain("pixel");
  });
});

describe("what it never produces", () => {
  const hostile = [
    "<script>alert(1)</script>",
    "<img src=x onerror=alert(1)>",
    '<a href="javascript:alert(1)">x</a>',
    "<iframe src=//evil></iframe>",
    "<style>body{display:none}</style>",
    "<svg onload=alert(1)>",
    "[click](javascript:alert(1))",
    "[click](JaVaScRiPt:alert(1))",
    "[click]( javascript:alert(1))",
    "[click](data:text/html,<script>alert(1)</script>)",
    "[click](vbscript:msgbox)",
    '[x](https://ok.example "title\\" onmouseover=\\"alert(1))',
    "![x](javascript:alert(1))",
    '![x" onerror="alert(1)](img/a.png)',
    "`<script>` and **<b onclick=alert(1)>bold</b>**",
    "| <img src=x onerror=1> | b |\n|---|---|\n| c | d |",
    "# <script>alert(1)</script>",
    "> <form action=//evil><input name=p></form>",
  ];

  for (const src of hostile) {
    it(`renders ${JSON.stringify(src)} as inert content`, () => {
      const out = dom(
        renderMarkdown(src, { resolveImage: (s) => (s.startsWith("img/") ? `/content?path=${s}` : null) }),
      );
      for (const el of out.querySelectorAll("*")) {
        expect(
          ["script", "iframe", "style", "object", "embed", "form", "input", "svg", "link", "meta", "base"],
          `a <${el.tagName.toLowerCase()}> element came out`,
        ).not.toContain(el.tagName.toLowerCase());
        for (const attr of el.getAttributeNames()) {
          expect(attr.startsWith("on"), `<${el.tagName.toLowerCase()} ${attr}> came out`).toBe(false);
        }
        for (const attr of ["href", "src"]) {
          const value = el.getAttribute(attr);
          if (value === null) continue;
          expect(
            /^\s*(javascript|vbscript|data):/i.test(value),
            `${attr}=${value} came out`,
          ).toBe(false);
        }
      }
    });
  }

  it("keeps the words of a raw HTML block, as text", () => {
    // Rendering raw HTML as text rather than dropping it: the operator should
    // see that the file contains markup, not a blank where it was.
    const out = dom(renderMarkdown("<div class=x>hello</div>", noImages));
    expect(out.querySelector("div.x")).toBeNull();
    expect(out.textContent).toContain("<div class=x>hello</div>");
  });
});

describe("what a hostile file cannot do to the shell", () => {
  // The renderer runs on the shell's main thread while the operator steps
  // through a folder, so a README crafted to be slow freezes every window.
  // Each of these used to take seconds to minutes at the 512 KiB preview
  // size; linear work is a few milliseconds. The budget is generous so a
  // loaded machine does not fail it, and still far under the quadratic time.
  const SIZE = 128 * 1024;
  const fill = (unit: string) => unit.repeat(Math.ceil(SIZE / unit.length));
  const hostile: Record<string, string> = {
    "unclosed brackets": fill("["),
    "unclosed image brackets": fill("!["),
    "links that never close their address": fill("[a]("),
    "strong emphasis that never closes": fill("**a "),
    "underscore emphasis that never closes": fill("__a "),
    "strikethrough that never closes": fill("~~a "),
    "links nested inside link labels": "[".repeat(SIZE / 8) + "a" + "](x)".repeat(SIZE / 8),
    "a list item continued across many blank lines": "- a" + "\n".repeat(SIZE) + "  b",
  };
  for (const [what, src] of Object.entries(hostile)) {
    it(`renders ${what} in linear time`, () => {
      const started = performance.now();
      const out = renderMarkdown(src, noImages);
      const took = performance.now() - started;
      expect(out.length).toBeGreaterThan(0);
      expect(took, `${what} took ${took.toFixed(0)} ms`).toBeLessThan(400);
    });
  }

  const deep: Record<string, string> = {
    "block quotes": ">".repeat(20_000) + " deepest",
    "spaced block quotes": "> ".repeat(20_000) + "deepest",
    "list items": "- ".repeat(20_000) + "deepest",
    "indented list items": Array.from({ length: 3000 }, (_, i) => "  ".repeat(i) + "- a").join("\n") + " deepest",
  };
  for (const [what, src] of Object.entries(deep)) {
    it(`renders thousands of nested ${what} without overflowing the stack, keeping the text`, () => {
      const out = dom(renderMarkdown(src, noImages));
      expect(out.textContent).toContain("deepest");
    });
  }

  it("still nests the shallow quotes and lists people write", () => {
    const out = dom(renderMarkdown("> > > three deep\n\n- a\n  - b\n    - c", noImages));
    expect(out.querySelector("blockquote blockquote blockquote")?.textContent?.trim()).toBe("three deep");
    expect(out.querySelector("ul ul ul li")?.textContent).toBe("c");
  });

  it("still keeps a list whole across blank lines between its items, as a loose list", () => {
    const out = dom(renderMarkdown("- a\n\n\n- b", noImages));
    expect(out.querySelectorAll("ul").length).toBe(1);
    expect([...out.querySelectorAll("ul > li > p")].map((p) => p.textContent)).toEqual(["a", "b"]);
  });

  it("still renders a badge: an image inside a link", () => {
    const out = dom(
      renderMarkdown("[![build](badge.svg)](https://ci.example.com)", {
        resolveImage: (src) => `/content?path=${src}`,
      }),
    );
    expect(out.querySelector("a[href='https://ci.example.com'] img")?.getAttribute("alt")).toBe("build");
  });

  it("still pairs emphasis the way it did, nearest closer first", () => {
    const out = dom(renderMarkdown("**a** and **b **c** __d__ x__y__ ~~e~~ ~~ f~~", noImages));
    expect([...out.querySelectorAll("strong")].map((s) => s.textContent)).toEqual(["a", "b **c", "d"]);
    expect([...out.querySelectorAll("del")].map((s) => s.textContent)).toEqual(["e"]);
    expect(out.textContent).toContain("x__y__");
  });
});
