// @vitest-environment happy-dom
import { describe, expect, it } from "vitest";

import { createUi, type UiEnv, type UiStore } from "./ui.ts";
import { breakpointFor, createViewport, type ViewportEnv } from "../shell/viewport.ts";
import type { BreakpointName } from "../tokens/index.ts";
import { harness, mountInShell } from "../testing/shell-harness.ts";
import App from "../App.vue";
import CompactShell from "../components/CompactShell.vue";
import ConfirmDialog from "../components/ConfirmDialog.vue";
import DesktopShell from "../components/DesktopShell.vue";
import NoticeStack from "../components/NoticeStack.vue";

/**
 * What the shell shows the operator, and at what size.
 *
 * Two modules, one promise. `ui.ts` owns the things that appear in front of
 * the operator and have to be either read or answered; `viewport.ts` owns
 * which layout those things appear in. Both are driven here through their
 * injected environments, so time and the window are the test's to move and
 * nothing depends on a real timer firing or a real browser resizing.
 *
 * Two shapes of assertion appear deliberately:
 *
 *   - Store-level, for the promises that are about state: an error that stays
 *     up, a confirmation that settles once, a mode that follows the pointer.
 *   - Mounted on the real shell from `shell-harness.ts`, for the promises the
 *     operator actually touches. This project has shipped finished stores that
 *     were never constructed, and a store test cannot tell the difference
 *     between "the notice is in the list" and "the notice is on the screen".
 *     The mounted tests close that gap: they fail if the notice list is right
 *     and the stack above it shows the wrong thing, and they fail if the shell
 *     stops building a viewport at all.
 *
 * Nothing here pins a pixel threshold. A breakpoint is a tuning decision and
 * the operator's promise is the *classification* — a cover screen is never
 * treated as a desktop — so the tests assert ordering and classification and
 * derive their own test widths from the classifier.
 */

/** Longer than any auto-dismissal worth having, without naming its length. */
const LONG_ENOUGH_MS = 10 * 60 * 1000;

interface UiRig {
  ui: UiStore;
  /** Move the clock and run whatever came due. */
  advance(ms: number): void;
}

function uiRig(): UiRig {
  let clock = 0;
  let timers: Array<{ at: number; fn: () => void; dead: boolean }> = [];

  const env: UiEnv = {
    delay(ms, fn) {
      const entry = { at: clock + ms, fn, dead: false };
      timers.push(entry);
      return () => {
        entry.dead = true;
      };
    },
  };

  return {
    ui: createUi(env),
    advance(ms) {
      const target = clock + ms;
      // Run in due order, because a task may schedule another.
      for (;;) {
        const next = timers
          .filter((t) => !t.dead && t.at <= target)
          .sort((a, b) => a.at - b.at)[0];
        if (!next) break;
        clock = Math.max(clock, next.at);
        next.dead = true;
        timers = timers.filter((t) => !t.dead);
        next.fn();
      }
      clock = target;
    },
  };
}

function texts(ui: UiStore): string[] {
  return ui.notices.value.map((n) => n.text);
}

describe("a notice the operator has not read", () => {
  it("clears itself when it was only informational", () => {
    const r = uiRig();
    r.ui.notify("info", "copied the path");

    r.advance(LONG_ENOUGH_MS);

    expect(texts(r.ui), "an informational notice piled up instead of clearing").toEqual([]);
  });

  it("stays up when something went wrong, until the operator takes it down", () => {
    // The asymmetry is the point: an error that disappears on its own is an
    // error that never got reported. A test that let both expire would pass
    // for a shell that silently swallows every failure.
    const r = uiRig();
    const id = r.ui.notify("error", "the node refused to move the window");

    r.advance(LONG_ENOUGH_MS);
    expect(texts(r.ui), "an error vanished before anyone could read it").toEqual([
      "the node refused to move the window",
    ]);

    r.ui.dismiss(id);
    expect(texts(r.ui), "an error could not be dismissed").toEqual([]);
  });

  it("is one of several, shown in the order they arrived", () => {
    // Errors, so the ordering is observed without time entering into it.
    const r = uiRig();
    r.ui.notify("error", "first");
    r.ui.notify("error", "second");
    r.ui.notify("error", "third");

    expect(texts(r.ui), "notices were reordered or dropped").toEqual([
      "first",
      "second",
      "third",
    ]);
  });
});

describe("dismissing a notice", () => {
  it("leaves the other notices alone", () => {
    const r = uiRig();
    r.ui.notify("error", "first");
    const middle = r.ui.notify("error", "second");
    r.ui.notify("error", "third");

    r.ui.dismiss(middle);

    expect(texts(r.ui), "dismissing one notice disturbed the others").toEqual([
      "first",
      "third",
    ]);
  });

  it("never takes down a later notice that inherited its place", () => {
    // The bug this is written for: ids drawn from the list's length rather
    // than from a counter, so a notice dismissed by hand leaves a pending
    // timer that later matches a different notice and takes *that* one down.
    const r = uiRig();
    const first = r.ui.notify("info", "copied the path");
    r.ui.dismiss(first);

    const second = r.ui.notify("error", "the node refused to move the window");
    expect(second, "a dismissed notice's id was handed out again").not.toBe(first);

    r.advance(LONG_ENOUGH_MS);

    expect(texts(r.ui), "a stale timer took down an unrelated error").toEqual([
      "the node refused to move the window",
    ]);
  });
});

describe("a confirmation the operator has to answer", () => {
  it("hands back exactly what was pressed, and clears the dialog", async () => {
    for (const answer of [true, false]) {
      const r = uiRig();
      const asked = r.ui.confirm({
        command: "files.delete",
        title: "Delete 214 messages?",
        detail: "This cannot be undone.",
        risk: "destructive",
        count: 214,
      });

      expect(r.ui.confirmation.value, "nothing was put in front of the operator").toBeTruthy();
      r.ui.confirmation.value!.resolve(answer);

      expect(await asked, `answering ${answer} resolved the other way`).toBe(answer);
      expect(r.ui.confirmation.value, "the dialog stayed up after being answered").toBeNull();
    }
  });

  it("settles once, even if the dialog answers twice", async () => {
    // A dialog can be answered twice for honest reasons — a double tap, a
    // keyboard and a click racing — and the command behind it must run once.
    const r = uiRig();
    const settled: boolean[] = [];
    const asked = r.ui.confirm({
      command: "files.delete",
      title: "Delete 214 messages?",
      detail: "This cannot be undone.",
      risk: "destructive",
    });
    void asked.then((ok) => settled.push(ok));

    const answer = r.ui.confirmation.value!.resolve;
    answer(true);
    // The first answer has to close the dialog, or the second press is a
    // press on a question the operator has already settled.
    expect(r.ui.confirmation.value, "the dialog was still up after being answered").toBeNull();
    answer(false);
    await asked;

    expect(settled, "the confirmation settled more than once, or with the second answer").toEqual([
      true,
    ]);
  });
});

describe("the notice stack the operator looks at", () => {
  it("shows the failure and names the command that caused it", async () => {
    const rig = harness();
    const { wrapper } = mountInShell(NoticeStack, { harness: rig });

    rig.shell.ui.notify("error", "the node refused to move the window", "shell.window.place");
    rig.shell.ui.notify("info", "copied the path");
    await wrapper.vm.$nextTick();

    expect(wrapper.text(), "the operator was not shown the failure").toContain(
      "the node refused to move the window",
    );
    expect(wrapper.text(), "the failure did not say which command it came from").toContain(
      "shell.window.place",
    );

    // Both are on screen; waiting has to clear one and keep the other, which
    // is also what makes the wait here load-bearing rather than decorative.
    rig.advance(LONG_ENOUGH_MS);
    await wrapper.vm.$nextTick();
    expect(wrapper.text(), "the error left the screen on its own").toContain(
      "the node refused to move the window",
    );
    expect(wrapper.text(), "an informational notice stayed on screen forever").not.toContain(
      "copied the path",
    );
  });

  it("takes down the notice whose button was pressed, and only that one", async () => {
    const rig = harness();
    const { wrapper } = mountInShell(NoticeStack, { harness: rig });

    rig.shell.ui.notify("error", "first failure");
    rig.shell.ui.notify("error", "second failure");
    await wrapper.vm.$nextTick();

    const buttons = wrapper.findAll("button");
    expect(buttons.length, "the notices had no way to be dismissed").toBe(2);
    await buttons[0]!.trigger("click");

    expect(wrapper.text(), "the pressed notice stayed on screen").not.toContain("first failure");
    expect(wrapper.text(), "pressing one notice's button took down another").toContain(
      "second failure",
    );
  });
});

describe("the confirmation dialog the operator answers", () => {
  it("states how much it will affect, and resolves what was pressed", async () => {
    for (const { label, expected } of [
      { label: "Do it", expected: true },
      { label: "Cancel", expected: false },
    ]) {
      const rig = harness();
      const { wrapper } = mountInShell(ConfirmDialog, { harness: rig });

      const asked = rig.shell.ui.confirm({
        command: "mail.delete",
        title: "Delete messages?",
        detail: "This cannot be undone.",
        risk: "destructive",
        count: 214,
      });
      await wrapper.vm.$nextTick();

      // The count is the cheapest protection against a command that matched
      // more than it was meant to, so it has to reach the screen.
      expect(wrapper.text(), "the operator was not told how much this affects").toContain("214");
      expect(wrapper.text(), "the dialog did not say what it was asking").toContain(
        "Delete messages?",
      );

      const button = wrapper.findAll("button").find((b) => b.text() === label);
      expect(button, `there was no ${label} button to press`).toBeTruthy();
      await button!.trigger("click");

      expect(await asked, `pressing ${label} answered the other way`).toBe(expected);
      await wrapper.vm.$nextTick();
      expect(
        wrapper.find('[role="alertdialog"]').exists(),
        "the dialog stayed up after being answered",
      ).toBe(false);
    }
  });
});

/** The classes the shell promises, narrowest first. */
const CLASSES: BreakpointName[] = [
  "phone",
  "foldFolded",
  "foldUnfolded",
  "tablet",
  "desktop",
];

const rank = (name: BreakpointName) => CLASSES.indexOf(name);

/**
 * A width that lands in the given class, asked of the classifier rather than
 * read off a constant, so retuning a threshold cannot fail these tests.
 */
function widthIn(name: BreakpointName): number {
  for (let w = 0; w <= 4000; w++) if (breakpointFor(w) === name) return w + 8;
  throw new Error(`no width is classified as ${name}`);
}

interface ViewportRig {
  env: ViewportEnv;
  /** Report a new size the way the browser's resize event does. */
  resizeTo(size: { w: number; h: number }, hinge?: number | null): void;
  /** The pointer changing under a running shell: a mouse reaching a tablet. */
  setCoarsePointer(coarse: boolean): void;
}

function viewportRig(
  options: {
    size?: { w: number; h: number };
    coarsePointer?: boolean;
    hinge?: number | null;
  } = {},
): ViewportRig {
  let size = options.size ?? { w: widthIn("desktop"), h: 900 };
  let hinge = options.hinge ?? null;
  let coarse = options.coarsePointer ?? false;
  const resizes = new Set<() => void>();
  const pointerHandlers: Array<(event: { matches: boolean }) => void> = [];

  const env: ViewportEnv = {
    media(query) {
      const coarseQuery = query.includes("pointer: coarse");
      return {
        get matches() {
          return coarseQuery ? coarse : false;
        },
        media: query,
        onchange: null,
        addEventListener(_type: string, fn: (event: { matches: boolean }) => void) {
          if (coarseQuery) pointerHandlers.push(fn);
        },
        removeEventListener: () => {},
        addListener: () => {},
        removeListener: () => {},
        dispatchEvent: () => false,
      } as unknown as MediaQueryList;
    },
    size: () => size,
    onResize(fn) {
      resizes.add(fn);
      return () => void resizes.delete(fn);
    },
    hingeFraction: () => hinge,
  };

  return {
    env,
    resizeTo(next, nextHinge) {
      size = next;
      if (nextHinge !== undefined) hinge = nextHinge;
      for (const fn of [...resizes]) fn();
    },
    setCoarsePointer(next) {
      coarse = next;
      for (const fn of [...pointerHandlers]) fn({ matches: next });
    },
  };
}

describe("the size classes", () => {
  it("run narrowest to widest, with none of them unreachable", () => {
    // Ordering and reachability together are what "the breakpoints are
    // ordered" means in practice: two thresholds in the wrong order leave one
    // class that no width can ever be in, which is how a layout silently
    // stops being used.
    const seen = new Set<BreakpointName>();
    let previous = -1;

    for (let w = 0; w <= 2400; w++) {
      const name = breakpointFor(w);
      const index = rank(name);
      expect(index, `${w}px was classified as something unnamed: ${name}`).toBeGreaterThanOrEqual(
        0,
      );
      expect(index, `the class went backwards as the screen grew, at ${w}px`).toBeGreaterThanOrEqual(
        previous,
      );
      previous = index;
      seen.add(name);
    }

    for (const name of CLASSES) {
      expect(seen.has(name), `no width is ever classified as ${name}`).toBe(true);
    }
  });
});

describe("the layout the shell is in", () => {
  it("follows the measured size, so a cover screen is never called a desktop", () => {
    const r = viewportRig({ size: { w: widthIn("desktop"), h: 900 } });
    const viewport = createViewport(r.env);

    expect(viewport.breakpoint.value).toBe("desktop");

    // Taller than any threshold on purpose: a classifier reading the height
    // would call this cover screen a desktop.
    r.resizeTo({ w: 320, h: 1600 });

    expect(
      rank(viewport.breakpoint.value),
      `a 320px screen was classified as ${viewport.breakpoint.value}`,
    ).toBeLessThan(rank("tablet"));
    expect(viewport.width.value, "the measured width was not picked up").toBe(320);
  });

  it("is compact on a touch screen however wide it is", () => {
    // A tablet is not a desktop just because it is big: desktop chrome puts
    // targets where a finger cannot hit them.
    const wide = { w: widthIn("desktop"), h: 900 };
    const touch = createViewport(viewportRig({ size: wide, coarsePointer: true }).env);
    const mouse = createViewport(viewportRig({ size: wide, coarsePointer: false }).env);

    expect(touch.mode.value, "a wide touch screen was treated as a desktop").toBe("compact");
    expect(mouse.mode.value, "a wide screen with a mouse was not a desktop").toBe("desktop");
  });

  it("changes when the pointer does, because docking happens mid-session", () => {
    const r = viewportRig({ size: { w: widthIn("desktop"), h: 900 }, coarsePointer: true });
    const viewport = createViewport(r.env);
    expect(viewport.mode.value).toBe("compact");

    r.setCoarsePointer(false);

    expect(viewport.mode.value, "a mouse arrived and the shell stayed compact").toBe("desktop");
  });

  it("obeys the operator's choice over the measurement, and gives it back", () => {
    const r = viewportRig({ size: { w: widthIn("desktop"), h: 900 } });
    const viewport = createViewport(r.env);
    expect(viewport.mode.value).toBe("desktop");

    viewport.override.value = "compact";
    expect(viewport.mode.value, "the operator's choice was ignored").toBe("compact");
    expect(viewport.suggestedMode.value, "the measurement was overwritten rather than overridden")
      .toBe("desktop");

    viewport.override.value = null;
    expect(viewport.mode.value, "clearing the override did not return to the measurement").toBe(
      "desktop",
    );
  });
});

describe("a foldable's hinge", () => {
  it("is where the split goes when there is one", () => {
    const viewport = createViewport(viewportRig({ hinge: 0.38 }).env);

    expect(viewport.hinge.value).toBe(0.38);
    expect(viewport.splitFraction.value, "the split ignored the hinge and creased a pane").toBe(
      0.38,
    );
  });

  it("is not a fault when there is none: the split goes down the middle", () => {
    const viewport = createViewport(viewportRig({ hinge: null }).env);

    expect(viewport.hinge.value).toBeNull();
    expect(viewport.splitFraction.value, "a device with no hinge got no usable split").toBe(0.5);
  });

  it("appears when the device unfolds, which arrives as a resize", () => {
    const r = viewportRig({ size: { w: 320, h: 900 }, hinge: null });
    const viewport = createViewport(r.env);

    r.resizeTo({ w: widthIn("foldUnfolded"), h: 900 }, 0.47);

    expect(rank(viewport.breakpoint.value), "unfolding did not change the class").toBeGreaterThan(
      rank("foldFolded"),
    );
    expect(viewport.splitFraction.value, "the hinge was not re-measured on unfolding").toBe(0.47);
  });
});

describe("a viewport that has been stopped", () => {
  it("ignores a resize reported after it, rather than reviving", () => {
    // A shell torn down and rebuilt — a hot reload, a second shell in a test
    // — leaves the old one subscribed if `stop` does not detach, and both
    // then answer for the same window.
    const r = viewportRig({ size: { w: widthIn("desktop"), h: 900 } });
    const viewport = createViewport(r.env);
    const before = viewport.width.value;

    viewport.stop();
    r.resizeTo({ w: 320, h: 1600 }, 0.4);

    expect(viewport.width.value, "a stopped viewport kept following the window").toBe(before);
    expect(viewport.splitFraction.value, "a stopped viewport kept re-measuring the hinge").toBe(
      0.5,
    );
  });
});

describe("the shell as the operator sees it", () => {
  it("renders its compact layout on a wide touch screen, and the override flips it", async () => {
    // The defect class this exists for: a module built, tested and never
    // constructed. A store-level mode is worth nothing if the shell above it
    // renders the desktop layout anyway, so this asserts what is on screen.
    const rig = harness({ size: { w: widthIn("desktop"), h: 900 }, coarsePointer: true });
    const { wrapper } = mountInShell(App, {
      harness: rig,
      props: { shell: rig.shell },
      shallow: true,
    });

    expect(wrapper.attributes("data-mode"), "a touch shell reported itself as a desktop").toBe(
      "compact",
    );
    expect(
      wrapper.findComponent(CompactShell).exists(),
      "the compact layout was not rendered on a touch screen",
    ).toBe(true);
    expect(
      wrapper.findComponent(DesktopShell).exists(),
      "the desktop layout was rendered on a touch screen",
    ).toBe(false);

    rig.shell.viewport.override.value = "desktop";
    await wrapper.vm.$nextTick();

    expect(
      wrapper.findComponent(DesktopShell).exists(),
      "the operator chose the desktop layout and did not get it",
    ).toBe(true);
    expect(wrapper.attributes("data-mode")).toBe("desktop");
  });
});

// ---------------------------------------------------------------------------
// defects the mutation pass turned up
// ---------------------------------------------------------------------------

describe("a second question while one is already on screen", () => {
  it("does not leave the first one waiting forever", async () => {
    // Found by mutation testing. `confirm()` assigns `confirmation.value`
    // unconditionally, so a second command's question replaces the first
    // one's and the first `resolve` is never called -- that promise never
    // settles. The caller is an `await` inside a command, so the command
    // hangs: a spinner that never stops, on a confirmation the operator
    // already answered, with nothing in any log.
    const { ui } = uiRig();

    const first = ui.confirm({ command: "a.destroy", title: "Confirm", detail: "first", risk: "destructive" });
    const second = ui.confirm({ command: "b.destroy", title: "Confirm", detail: "second", risk: "destructive" });

    // Answer whatever is on screen. Whichever question that is, the one it
    // displaced must not be left pending.
    ui.confirmation.value?.resolve(true);

    const settled = await Promise.race([
      Promise.all([first, second]).then(() => "both settled"),
      new Promise((r) => setTimeout(() => r("HUNG"), 30)),
    ]);
    expect(settled, "a displaced confirmation never settled").toBe("both settled");
  });

  it("does not answer a question the operator was never shown", async () => {
    // The other half: resolving the displaced one as `true` would be worse
    // than hanging. A destructive command must never be treated as confirmed
    // because a second dialog happened to open.
    const { ui } = uiRig();

    const first = ui.confirm({ command: "a.destroy", title: "Confirm", detail: "first", risk: "destructive" });
    void ui.confirm({ command: "b.destroy", title: "Confirm", detail: "second", risk: "destructive" });
    ui.confirmation.value?.resolve(true);

    expect(await first, "a command nobody confirmed was reported as confirmed").toBe(false);
  });
});

describe("a viewport that has been stopped", () => {
  it("lets go of every listener, not just the resize one", () => {
    // Found by mutation testing. `stop()` detaches the resize subscription
    // and leaves the `pointer: coarse` and `display-mode` change listeners
    // attached, so a disposed viewport keeps mutating its own state -- and
    // every hot reload or shell rebuild leaves another pair behind.
    const attached: string[] = [];
    const removed: string[] = [];
    const env: ViewportEnv = {
      media(query) {
        return {
          matches: false,
          media: query,
          onchange: null,
          addEventListener: () => void attached.push(query),
          removeEventListener: () => void removed.push(query),
          addListener: () => {},
          removeListener: () => {},
          dispatchEvent: () => false,
        } as unknown as MediaQueryList;
      },
      size: () => ({ w: 1600, h: 900 }),
      onResize: () => () => {},
      hingeFraction: () => null,
    };

    const viewport = createViewport(env);
    viewport.stop();

    expect(attached.length, "nothing was listening to begin with").toBeGreaterThan(0);
    expect(removed.sort(), "a stopped viewport is still listening").toEqual(attached.sort());
  });
});
