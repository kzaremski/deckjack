import { computed, ref, shallowRef, type ComputedRef, type Ref } from "vue";

import type { CommandSpec, Risk } from "../protocol.ts";

/**
 * The command registry: everything the operator, the palette, the keyboard,
 * the CLI and an agent can dispatch, in one list.
 *
 * "Everything is a command" is only real if the UI has no second path. So a
 * dock button calls `dispatch("app.open.files")` rather than
 * `windows.open("files")`, and the palette is a view over this registry
 * rather than a hand-written menu.
 *
 * Shell commands are registered locally because the shell *is* their backend.
 * App and adapter commands arrive over the bus as `CommandSpec`s and are
 * dispatched back to it; both kinds live here and look identical in the
 * palette, which is what makes a new adapter show up with no UI work.
 */

export interface CommandEntry extends CommandSpec {
  /**
   * Runs it. Local for shell commands, a bus call for everything else.
   *
   * `params` is optional because most commands act on the focused thing and
   * need nothing. The ones that do need it -- Quick Look is meaningless
   * without a path -- used to have no way to be told, so they could only be
   * driven by reaching into a panel's selection: the one caller an agent and
   * the CLI do not have. Principle 3 says every caller dispatches the same
   * command, and that is only true if every caller can pass the same
   * parameters.
   */
  run(params?: Record<string, unknown>): void | Promise<void>;
  /** Re-evaluated when the palette opens, not cached. */
  available?: () => boolean;
}

export interface Scored {
  entry: CommandEntry;
  score: number;
  /** Indices of the matched characters in the title, for highlighting. */
  hits: number[];
}

export interface CommandsEnv {
  storage: { read(key: string): string | null; write(key: string, v: string): void };
}

const RECENT_KEY = "deckjack.recentCommands";

export interface CommandsStore {
  all: ComputedRef<CommandEntry[]>;
  byId(id: string): CommandEntry | null;
  register(entries: CommandEntry[]): () => void;
  /** Replaces everything the bus provided, keeping local shell commands. */
  adopt(specs: CommandSpec[], call: (id: string, node?: string) => Promise<void>): void;
  dispatch(id: string, params?: Record<string, unknown>): Promise<void>;
  search(query: string, limit?: number): Scored[];
  lastError: Ref<string | null>;
}

/**
 * Subsequence match with a deliberately simple score: consecutive characters
 * and word starts count, everything else is a tie broken by recency. A
 * cleverer ranking is not worth the surprise of a palette that reorders
 * itself for reasons the operator cannot see.
 */
export function fuzzy(query: string, text: string): { score: number; hits: number[] } | null {
  if (query === "") return { score: 0, hits: [] };
  const q = query.toLowerCase();
  const t = text.toLowerCase();
  const hits: number[] = [];
  let score = 0;
  let from = 0;
  let previous = -2;

  for (const ch of q) {
    if (ch === " ") continue;
    const at = t.indexOf(ch, from);
    if (at === -1) return null;
    hits.push(at);
    score += at === previous + 1 ? 8 : 1;
    const before = at > 0 ? t[at - 1] : " ";
    if (before === " " || before === "." || before === "/" || at === 0) score += 6;
    previous = at;
    from = at + 1;
  }
  // A short title that matched is a better answer than a long one that did.
  return { score: score - Math.floor(text.length / 12), hits };
}

export function createCommands(env: CommandsEnv): CommandsStore {
  const local = shallowRef<CommandEntry[]>([]);
  const remote = shallowRef<CommandEntry[]>([]);
  const lastError = ref<string | null>(null);

  let recent: string[] = [];
  try {
    const raw = env.storage.read(RECENT_KEY);
    recent = raw ? (JSON.parse(raw) as string[]) : [];
  } catch {
    recent = [];
  }

  const all = computed(() => [...local.value, ...remote.value]);

  function remember(id: string) {
    recent = [id, ...recent.filter((x) => x !== id)].slice(0, 40);
    env.storage.write(RECENT_KEY, JSON.stringify(recent));
  }

  function find(id: string): CommandEntry | null {
    return all.value.find((c) => c.id === id) ?? null;
  }

  return {
    all,
    byId: find,
    register(entries) {
      local.value = [...local.value, ...entries];
      const ids = new Set(entries.map((e) => e.id));
      return () => {
        local.value = local.value.filter((e) => !ids.has(e.id));
      };
    },
    adopt(specs, call) {
      remote.value = specs.map((spec) => ({
        ...spec,
        run: () => call(spec.id, spec.node),
      }));
    },
    async dispatch(id, params) {
      const entry = find(id);
      if (!entry) {
        // A missing command is a wiring bug, not something to swallow: a
        // shortcut that silently does nothing is the worst kind.
        lastError.value = `no command ${id}`;
        return;
      }
      if (entry.available && !entry.available()) return;
      remember(id);
      try {
        await entry.run(params);
        lastError.value = null;
      } catch (e) {
        lastError.value = e instanceof Error ? e.message : String(e);
      }
    },
    search(query, limit = 12) {
      const available = all.value.filter((c) => !c.available || c.available());
      if (query.trim() === "") {
        const order = new Map(recent.map((id, i) => [id, i]));
        return [...available]
          .sort((a, b) => (order.get(a.id) ?? 999) - (order.get(b.id) ?? 999))
          .slice(0, limit)
          .map((entry) => ({ entry, score: 0, hits: [] }));
      }
      const scored: Scored[] = [];
      for (const entry of available) {
        const haystack = [entry.title, entry.id, ...(entry.keywords ?? [])];
        let bestScore = -Infinity;
        let bestHits: number[] = [];
        for (let index = 0; index < haystack.length; index += 1) {
          const hit = fuzzy(query, haystack[index] ?? "");
          if (!hit) continue;
          // A match in the title beats the same match in a keyword.
          const score = hit.score - index * 3;
          if (score > bestScore) {
            bestScore = score;
            // Only the title's indices can be highlighted, so a keyword match
            // scores but highlights nothing.
            bestHits = index === 0 ? hit.hits : [];
          }
        }
        if (bestScore === -Infinity) continue;
        const bonus = recent.includes(entry.id) ? 4 : 0;
        scored.push({ entry, score: bestScore + bonus, hits: bestHits });
      }
      return scored.sort((a, b) => b.score - a.score).slice(0, limit);
    },
    lastError,
  };
}

/** How a risk level is labeled in the palette and on a confirmation. */
export function riskLabel(risk: Risk): string {
  switch (risk) {
    case "read_only":
      return "reads";
    case "write":
      return "writes";
    case "cross_project":
      return "touches another project";
    case "outbound":
      return "sends something out";
    case "destructive":
      return "destructive";
  }
}
