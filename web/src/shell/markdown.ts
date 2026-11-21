/**
 * Markdown to HTML for Quick Look, safe by construction.
 *
 * # Why not a library plus a sanitizer
 *
 * The usual answer is a Markdown library that passes raw HTML through, and a
 * sanitizer (DOMPurify) run over the result to take the dangerous parts back
 * out. That is two dependencies and an allow-list to keep correct, on the one
 * surface where getting it wrong runs a downloaded file's script with the
 * operator's session -- and it renders the HTML a README embeds, which a
 * preview has no need to.
 *
 * This renderer instead never emits anything it did not build itself:
 *
 * - Every piece of the source reaches the output through {@link escape},
 *   so `<script>` in a README is the text "<script>", in a paragraph.
 * - Elements come from a fixed list in this file, and the only attributes
 *   carrying source text are `href`, `src`, `alt` and `title`, each escaped,
 *   and `href`/`src` only after {@link safeHref} / the caller's
 *   `resolveImage` accepted the address.
 * - Raw HTML in the source is shown as text rather than dropped, so the
 *   operator can see the file contains markup.
 *
 * # Bounded work
 *
 * It runs on the shell's main thread, on a file anyone could have put in the
 * working directory, so every pass is linear in its input: brackets are
 * matched once per line rather than rescanned from every `[`, emphasis is
 * paired against a precomputed "next closer" table rather than a lazy regex
 * that rescans to the end of the line from every opener, link addresses
 * share one scanning budget per line, and nesting (quotes, lists, links in
 * labels) stops at a fixed depth, past which the rest is shown as text.
 *
 * What it covers is the Markdown people write in READMEs and notes:
 * ATX and setext headings, paragraphs, emphasis, strikethrough, code spans,
 * fenced and indented code, block quotes, nested lists, tables, rules, links
 * and images. It is not a full CommonMark implementation and does not try to
 * be; anything it does not recognize is shown as the text it is.
 */

export interface MarkdownOptions {
  /**
   * The address to load a relative image from, or null to show its alt text
   * instead. Never called for an absolute or protocol-relative address: a
   * remote image in a previewed file is a tracking pixel.
   */
  resolveImage(src: string): string | null;
}

export function escape(text: string): string {
  return text
    .replace(/&/g, "&amp;")
    .replace(/</g, "&lt;")
    .replace(/>/g, "&gt;")
    .replace(/"/g, "&quot;")
    .replace(/'/g, "&#39;");
}

/**
 * Whether an address may become a clickable link: http, https or mailto, and
 * nothing else. The scheme is read after removing whitespace and control
 * characters, because browsers ignore those inside a scheme and
 * `java\tscript:` is a known way past a naive check.
 */
export function safeHref(url: string): string | null {
  const compact = url.replace(/[\u0000- \u007f-\u009f]/g, "");
  const scheme = /^([a-z][a-z0-9+.-]*):/i.exec(compact);
  if (!scheme) return null;
  const name = scheme[1]!.toLowerCase();
  return name === "http" || name === "https" || name === "mailto" ? url.trim() : null;
}

/** An address with no scheme and no host: a file next to this one. */
function isRelative(url: string): boolean {
  const compact = url.replace(/[\u0000- \u007f-\u009f]/g, "");
  if (compact.startsWith("//")) return false;
  return !/^[a-z][a-z0-9+.-]*:/i.test(compact);
}

export function renderMarkdown(source: string, options: MarkdownOptions): string {
  // NUL is the placeholder delimiter below; a source carrying its own would
  // otherwise be able to forge a placeholder.
  const text = source.replace(/\r\n?/g, "\n").replace(/\u0000/g, "�");
  return blocks(text.split("\n"), options, 0);
}

/**
 * How deep quotes and lists nest before the rest is shown as plain text.
 * Far past anything written by hand; it exists so a file of 20,000 `>`
 * neither overflows the stack nor costs depth x length work.
 */
const MAX_NESTING = 16;

/** How deep links nest inside link labels: a badge is an image in a link. */
const MAX_LINK_NESTING = 2;

// -- blocks -------------------------------------------------------------------

const FENCE = /^ {0,3}(`{3,}|~{3,})\s*([^`\s]*)[^`]*$/;
const HEADING = /^ {0,3}(#{1,6})(?:[ \t]+(.*?))?(?:[ \t]+#+)?[ \t]*$/;
const RULE = /^ {0,3}([-*_])(?:[ \t]*\1){2,}[ \t]*$/;
const QUOTE = /^ {0,3}> ?(.*)$/;
const ITEM = /^( *)([-*+]|\d{1,9}[.)])([ \t]+|$)(.*)$/;
const TABLE_DELIMITER = /^\s*\|?\s*:?-+:?\s*(\|\s*:?-+:?\s*)*\|?\s*$/;
const SETEXT = /^ {0,3}(=+|-+)[ \t]*$/;

function isBlank(line: string): boolean {
  return line.trim() === "";
}

/** Whether a line starts a block that interrupts a paragraph. */
function interrupts(line: string): boolean {
  return (
    FENCE.test(line) ||
    HEADING.test(line) ||
    RULE.test(line) ||
    QUOTE.test(line) ||
    /^ {0,3}([-*+]|1[.)])[ \t]+\S/.test(line)
  );
}

function blocks(lines: string[], options: MarkdownOptions, depth: number): string {
  if (depth >= MAX_NESTING) {
    // Too deep to be a document anyone wrote: the words, as one paragraph.
    const words = lines.filter((l) => !isBlank(l));
    return words.length ? `<p>${escape(words.join("\n"))}</p>` : "";
  }
  const out: string[] = [];
  let i = 0;

  while (i < lines.length) {
    const line = lines[i]!;

    if (isBlank(line)) {
      i += 1;
      continue;
    }

    const fence = FENCE.exec(line);
    if (fence) {
      const marker = fence[1]!;
      const indent = line.length - line.trimStart().length;
      const body: string[] = [];
      i += 1;
      while (i < lines.length) {
        const l = lines[i]!;
        if (l.trim().startsWith(marker[0]!.repeat(marker.length)) && l.trim().replace(/[`~]/g, "") === "") {
          i += 1;
          break;
        }
        body.push(l.startsWith(" ".repeat(indent)) ? l.slice(indent) : l.trimStart());
        i += 1;
      }
      const lang = fence[2] ? ` class="language-${escape(fence[2])}"` : "";
      out.push(`<pre><code${lang}>${escape(body.join("\n"))}</code></pre>`);
      continue;
    }

    if (line.startsWith("    ") || line.startsWith("\t")) {
      const body: string[] = [];
      while (i < lines.length && (lines[i]!.startsWith("    ") || lines[i]!.startsWith("\t") || isBlank(lines[i]!))) {
        body.push(lines[i]!.replace(/^( {4}|\t)/, ""));
        i += 1;
      }
      while (body.length && isBlank(body.at(-1)!)) body.pop();
      out.push(`<pre><code>${escape(body.join("\n"))}</code></pre>`);
      continue;
    }

    const heading = HEADING.exec(line);
    if (heading) {
      const level = heading[1]!.length;
      out.push(`<h${level}>${inline(heading[2] ?? "", options)}</h${level}>`);
      i += 1;
      continue;
    }

    if (RULE.test(line)) {
      out.push("<hr>");
      i += 1;
      continue;
    }

    if (QUOTE.test(line)) {
      const inner: string[] = [];
      while (i < lines.length && !isBlank(lines[i]!)) {
        const q = QUOTE.exec(lines[i]!);
        // A line without `>` continues the quote's paragraph (laziness).
        inner.push(q ? q[1]! : lines[i]!);
        i += 1;
      }
      out.push(`<blockquote>${blocks(inner, options, depth + 1)}</blockquote>`);
      continue;
    }

    const item = ITEM.exec(line);
    if (item) {
      i = list(lines, i, out, options, depth);
      continue;
    }

    if (line.includes("|") && i + 1 < lines.length && TABLE_DELIMITER.test(lines[i + 1]!) && lines[i + 1]!.includes("-")) {
      i = table(lines, i, out, options);
      continue;
    }

    // A paragraph, possibly turned into a heading by a setext underline.
    const para: string[] = [line];
    i += 1;
    let setext = 0;
    while (i < lines.length && !isBlank(lines[i]!)) {
      const l = lines[i]!;
      const under = SETEXT.exec(l);
      if (under) {
        setext = under[1]!.startsWith("=") ? 1 : 2;
        i += 1;
        break;
      }
      if (interrupts(l)) break;
      para.push(l);
      i += 1;
    }
    const content = paragraphInline(para, options);
    out.push(setext ? `<h${setext}>${content}</h${setext}>` : `<p>${content}</p>`);
  }

  return out.join("\n");
}

function paragraphInline(lines: string[], options: MarkdownOptions): string {
  return lines
    .map((l, n) => {
      const last = n === lines.length - 1;
      const hard = !last && (/ {2,}$/.test(l) || /\\$/.test(l));
      const trimmed = l.trim().replace(/\\$/, "");
      return inline(trimmed, options) + (hard ? "<br>" : "");
    })
    .join("\n");
}

function list(
  lines: string[],
  start: number,
  out: string[],
  options: MarkdownOptions,
  depth: number,
): number {
  const first = ITEM.exec(lines[start]!)!;
  const indent = first[1]!.length;
  const ordered = /\d/.test(first[2]!);
  const startNumber = ordered ? parseInt(first[2]!, 10) : 1;
  const items: string[][] = [];
  let loose = false;
  let i = start;

  while (i < lines.length) {
    const m = ITEM.exec(lines[i]!);
    if (!m || m[1]!.length !== indent || /\d/.test(m[2]!) !== ordered) break;
    // Content starts after the marker and its spacing; continuation lines
    // indented at least that far belong to this item.
    const offset = m[1]!.length + m[2]!.length + Math.max(1, Math.min(m[3]!.length, 4));
    const body: string[] = [m[4]!];
    i += 1;
    while (i < lines.length) {
      const l = lines[i]!;
      if (isBlank(l)) {
        // A blank line inside a list keeps the item going only if what follows
        // is indented under it; otherwise the item (or the list) ends here.
        // The whole run of blank lines is decided at once: looking ahead
        // again from each of them is quadratic in the length of the run.
        const after = nextNonBlank(lines, i);
        if (after < lines.length && leading(lines[after]!) >= offset) {
          for (; i < after; i += 1) body.push("");
          loose = true;
          continue;
        }
        break;
      }
      if (leading(l) >= offset) {
        body.push(l.slice(offset));
        i += 1;
        continue;
      }
      // Lazy continuation of the item's paragraph.
      if (!ITEM.test(l) && !interrupts(l)) {
        body.push(l.trim());
        i += 1;
        continue;
      }
      break;
    }
    items.push(body);
    // A blank line between items makes the list loose.
    if (i < lines.length && isBlank(lines[i]!)) {
      const after = nextNonBlank(lines, i);
      const nm = after < lines.length ? ITEM.exec(lines[after]!) : null;
      if (nm && nm[1]!.length === indent && /\d/.test(nm[2]!) === ordered) {
        loose = true;
        i = after;
        continue;
      }
      break;
    }
  }

  const rendered = items.map((body) => {
    const html = blocks(body, options, depth + 1);
    // A tight item's single paragraph is shown without its <p>, the way every
    // renderer does, so a list of one-liners is not double-spaced.
    return loose ? html : html.replace(/^<p>([\s\S]*?)<\/p>/, "$1");
  });
  const tag = ordered ? "ol" : "ul";
  const startAttr = ordered && startNumber !== 1 ? ` start="${startNumber}"` : "";
  out.push(`<${tag}${startAttr}>${rendered.map((r) => `<li>${r}</li>`).join("")}</${tag}>`);
  return i;
}

/** The index of the first non-blank line at or after `from`, or the length. */
function nextNonBlank(lines: string[], from: number): number {
  let j = from;
  while (j < lines.length && isBlank(lines[j]!)) j += 1;
  return j;
}

function leading(line: string): number {
  return line.length - line.trimStart().length;
}

function cells(row: string): string[] {
  let r = row.trim();
  if (r.startsWith("|")) r = r.slice(1);
  if (r.endsWith("|") && !r.endsWith("\\|")) r = r.slice(0, -1);
  // Split on pipes that are not escaped.
  const parts: string[] = [];
  let current = "";
  for (let k = 0; k < r.length; k += 1) {
    if (r[k] === "\\" && r[k + 1] === "|") {
      current += "|";
      k += 1;
    } else if (r[k] === "|") {
      parts.push(current);
      current = "";
    } else {
      current += r[k];
    }
  }
  parts.push(current);
  return parts.map((c) => c.trim());
}

function table(lines: string[], start: number, out: string[], options: MarkdownOptions): number {
  const head = cells(lines[start]!);
  const aligns = cells(lines[start + 1]!).map((d) =>
    d.startsWith(":") && d.endsWith(":") ? "center" : d.endsWith(":") ? "right" : d.startsWith(":") ? "left" : "",
  );
  const cls = (n: number) => (aligns[n] ? ` class="align-${aligns[n]}"` : "");
  let i = start + 2;
  const rows: string[] = [];
  while (i < lines.length && !isBlank(lines[i]!) && lines[i]!.includes("|")) {
    const row = cells(lines[i]!);
    rows.push(
      `<tr>${head.map((_, n) => `<td${cls(n)}>${inline(row[n] ?? "", options)}</td>`).join("")}</tr>`,
    );
    i += 1;
  }
  out.push(
    `<table><thead><tr>${head.map((h, n) => `<th${cls(n)}>${inline(h, options)}</th>`).join("")}</tr></thead>` +
      `<tbody>${rows.join("")}</tbody></table>`,
  );
  return i;
}

// -- inlines ------------------------------------------------------------------

/**
 * Inline Markdown. Code spans, escapes, links and images are cut out first
 * and replaced with numbered placeholders, the remaining text is escaped,
 * emphasis is applied to the escaped text, and the placeholders are put
 * back. Escaping before any tag is added is what makes the emphasis pass
 * safe: by then there is no `<` left in the text to misuse.
 */
function inline(text: string, options: MarkdownOptions, linkDepth = 0): string {
  const closing = matchBrackets(text);
  // Shared by every link address scanned in this text. Each successful scan
  // consumes what it covers, so a real document stays far inside it; a line
  // of `[a](` repeated is what runs it out, and the rest is then text.
  const budget = { steps: 4 * text.length + 1024 };
  const held: string[] = [];
  const hold = (html: string) => `\u0000${held.push(html) - 1}\u0000`;
  let s = "";
  let k = 0;

  while (k < text.length) {
    const c = text[k]!;

    if (c === "\\" && k + 1 < text.length && /[!-/:-@[-`{-~]/.test(text[k + 1]!)) {
      s += hold(escape(text[k + 1]!));
      k += 2;
      continue;
    }

    if (c === "`") {
      const run = /^`+/.exec(text.slice(k))![0];
      const close = text.indexOf(run, k + run.length);
      if (close !== -1) {
        let code = text.slice(k + run.length, close);
        if (/^ .* $/.test(code)) code = code.slice(1, -1);
        s += hold(`<code>${escape(code)}</code>`);
        k = close + run.length;
        continue;
      }
      s += run;
      k += run.length;
      continue;
    }

    if (c === "<") {
      const auto = /^<((?:https?|mailto):[^\s<>]+)>/i.exec(text.slice(k));
      if (auto) {
        const href = safeHref(auto[1]!);
        s += href ? hold(anchor(href, escape(auto[1]!))) : auto[0];
        k += auto[0].length;
        continue;
      }
    }

    const image = c === "!" && text[k + 1] === "[";
    if (c === "[" || image) {
      const link = parseLink(text, image ? k + 1 : k, closing, budget);
      if (link) {
        if (image) {
          const url = isRelative(link.dest) && link.dest !== "" ? options.resolveImage(link.dest) : null;
          s += hold(
            url
              ? `<img src="${escape(url)}" alt="${escape(link.label)}"${link.title ? ` title="${escape(link.title)}"` : ""} loading="lazy">`
              : `<span class="md-image" title="${escape(link.dest)}">${escape(link.label || link.dest)}</span>`,
          );
        } else {
          const label =
            linkDepth < MAX_LINK_NESTING ? inline(link.label, options, linkDepth + 1) : escape(link.label);
          const href = safeHref(link.dest);
          s += hold(
            href
              ? anchor(href, label, link.title)
              : `<span class="md-link" title="${escape(link.dest)}">${label}</span>`,
          );
        }
        k = link.end;
        continue;
      }
    }

    s += c;
    k += 1;
  }

  let html = escape(s);
  // `*` and `_` cannot cross their own delimiter, so their regexes are linear;
  // the two-character delimiters are paired by `pairDelimited`.
  html = pairDelimited(html, "**", "strong", false);
  html = pairDelimited(html, "__", "strong", true);
  html = html
    .replace(/\*(?=[^\s*])([^*]*?[^\s*])\*/g, "<em>$1</em>")
    .replace(/\*(?=[^\s*])([^\s*])\*/g, "<em>$1</em>")
    .replace(/(^|[^\w])_(?=[^\s_])([^_]*?[^\s_]|[^\s_])_(?!\w)/g, "$1<em>$2</em>");
  html = pairDelimited(html, "~~", "del", false);
  return html.replace(/\u0000(\d+)\u0000/g, (_, n: string) => held[Number(n)] ?? "");
}

const WORD = /\w/;
const SPACE = /\s/;

/**
 * `D(?=\S)([\s\S]*?\S)D` replaced with `<tag>$1</tag>`, globally -- the
 * same pairs, nearest closer first -- in linear time. As a lazy regex, every
 * opener without a closer rescans to the end of the text, which is quadratic
 * on a line of `**a ` repeated. Here the closers are found once, from the
 * right, so each opener looks its closer up.
 *
 * `bounded` adds the word-boundary rules of `__`: no word character just
 * before the opener (`(^|[^\w])`) or just after the closer (`(?!\w)`).
 */
function pairDelimited(text: string, delim: string, tag: string, bounded: boolean): string {
  const n = text.length;
  const d = delim.length;
  // next[i]: the first closer at or after i, or -1. A closer is the
  // delimiter right after a non-space, and (bounded) not before a word.
  const next = new Int32Array(n + 1).fill(-1);
  for (let c = n - d; c >= 0; c -= 1) {
    const closes =
      text.startsWith(delim, c) &&
      c > 0 &&
      !SPACE.test(text[c - 1]!) &&
      !(bounded && c + d < n && WORD.test(text[c + d]!));
    next[c] = closes ? c : next[c + 1]!;
  }
  let out = "";
  let from = 0;
  let o = 0;
  while (o <= n - d) {
    const opens =
      text.startsWith(delim, o) &&
      o + d < n &&
      !SPACE.test(text[o + d]!) &&
      !(bounded && o > 0 && WORD.test(text[o - 1]!));
    // At least one character of content, so the closer starts past o + d.
    const c = opens && o + d + 1 <= n ? next[o + d + 1]! : -1;
    if (c === -1) {
      o += 1;
      continue;
    }
    out += text.slice(from, o) + `<${tag}>${text.slice(o + d, c)}</${tag}>`;
    from = o = c + d;
  }
  return out + text.slice(from);
}

/**
 * For each `[` in `text`, the index of the `]` that closes it, counting
 * nesting and skipping backslash-escaped characters; absent when unclosed.
 * One pass, so a line of 100,000 `[` costs 100,000 steps rather than
 * 100,000 scans to the end of the line.
 */
function matchBrackets(text: string): Map<number, number> {
  const closing = new Map<number, number>();
  const open: number[] = [];
  for (let k = 0; k < text.length; k += 1) {
    const c = text[k];
    if (c === "\\") k += 1;
    else if (c === "[") open.push(k);
    else if (c === "]" && open.length) closing.set(open.pop()!, k);
  }
  return closing;
}

function anchor(href: string, labelHtml: string, title?: string): string {
  const t = title ? ` title="${escape(title)}"` : "";
  return `<a href="${escape(href)}"${t} target="_blank" rel="noopener noreferrer">${labelHtml}</a>`;
}

/**
 * `[label](dest "title")` starting at the `[` at `at`, or null. `closing` is
 * `matchBrackets(text)`; `budget` bounds the address scanning shared by
 * every call on the same text.
 */
function parseLink(
  text: string,
  at: number,
  closing: Map<number, number>,
  budget: { steps: number },
): { label: string; dest: string; title?: string; end: number } | null {
  const k = closing.get(at);
  if (k === undefined || text[k + 1] !== "(") return null;
  const label = text.slice(at + 1, k);
  let j = k + 2;
  let parens = 1;
  let inQuote: string | null = null;
  for (; j < text.length; j += 1) {
    if (--budget.steps < 0) return null;
    const c = text[j]!;
    if (c === "\\") {
      j += 1;
      continue;
    }
    if (inQuote) {
      if (c === inQuote) inQuote = null;
      continue;
    }
    if ((c === '"' || c === "'") && /\s/.test(text[j - 1] ?? "")) inQuote = c;
    else if (c === "(") parens += 1;
    else if (c === ")") {
      parens -= 1;
      if (parens === 0) break;
    }
  }
  if (parens !== 0) return null;
  const inside = text.slice(k + 2, j).trim();
  let dest: string;
  let rest: string;
  if (inside.startsWith("<")) {
    const close = inside.indexOf(">");
    if (close === -1) return null;
    dest = inside.slice(1, close);
    rest = inside.slice(close + 1).trim();
  } else {
    const space = inside.search(/\s/);
    dest = space === -1 ? inside : inside.slice(0, space);
    rest = space === -1 ? "" : inside.slice(space).trim();
  }
  const titled = /^(["'])([\s\S]*)\1$/.exec(rest);
  return { label, dest, title: titled ? titled[2] : undefined, end: j + 1 };
}
