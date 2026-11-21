// @vitest-environment happy-dom
import { afterEach, describe, expect, it } from "vitest";
import { flushPromises, type VueWrapper } from "@vue/test-utils";

import ConfirmDialog from "./ConfirmDialog.vue";
import { mountInShell, type Harness } from "../testing/shell-harness.ts";

/**
 * The confirmation sheet, on the real shell: the node asks, the sheet shows
 * what the node said the call acts on, and the operator's answer goes back as
 * the node's own ticket.
 *
 * Driven through the real bus and the real wiring between the bus and the
 * dialog (`bindConfirmations`), so a target the node sends that never reaches
 * the sheet fails here rather than shipping as a dialog that asks blind.
 */

const TICKET = "0123456789abcdef0123456789abcdef";

let mounted: { wrapper: VueWrapper; rig: Harness } | null = null;

afterEach(() => {
  mounted?.wrapper.unmount();
  mounted?.rig.dispose();
  mounted = null;
});

/** Start a call and answer it with the node's question. */
async function asked(target: string) {
  mounted = mountInShell(ConfirmDialog);
  const { rig } = mounted;
  rig.connect();
  const call = rig.shell.bus.call("files.trash.empty", { ids: ["x"] });
  const invoke = rig
    .socket()
    .frames()
    .filter((f) => f.type === "invoke")
    .at(-1)!;
  rig.socket().deliver({
    type: "response",
    id: invoke.id,
    outcome: {
      status: "ok",
      value: {
        status: "needs_confirmation",
        prompt: `Empty the trash: ${target}. This removes or overwrites data and cannot be undone.`,
        target,
        risk: "destructive",
        ticket: TICKET,
      },
    },
    elapsed_ms: 1,
  });
  await flushPromises();
  return { ...mounted, call };
}

describe("the confirmation sheet", () => {
  it("shows the target the node rendered, set apart from the sentence", async () => {
    const { wrapper } = await asked("/thesis/chapter-3.md");

    const target = wrapper.find('[data-testid="confirm-target"]');
    expect(target.exists(), "the sheet did not show what the call acts on").toBe(true);
    expect(target.text()).toBe("/thesis/chapter-3.md");
  });

  it("shows a target as text, never as markup", async () => {
    // A file can be named anything, and the target is built from params.
    const { wrapper } = await asked('/<img src=x onerror="alert(1)">.txt');

    expect(wrapper.find('[data-testid="confirm-target"]').text()).toBe(
      '/<img src=x onerror="alert(1)">.txt',
    );
    expect(wrapper.find("img").exists(), "a file name became an element").toBe(false);
  });

  it("answers yes with the node's ticket, for the call it asked about", async () => {
    const { wrapper, rig, call } = await asked("/thesis/chapter-3.md");

    const go = wrapper.findAll("button").find((b) => b.text() === "Do it");
    expect(go, "no button to confirm with").toBeDefined();
    await go!.trigger("click");
    await flushPromises();

    const confirm = rig
      .socket()
      .frames()
      .filter((f) => f.type === "confirm")
      .at(-1);
    expect(confirm).toMatchObject({
      ticket: TICKET,
      command: "files.trash.empty",
      params: { ids: ["x"] },
    });
    rig.socket().deliver({
      type: "response",
      id: confirm!.id,
      outcome: { status: "ok", value: { removed: 1 } },
      elapsed_ms: 1,
    });
    expect(await call).toMatchObject({ status: "ok" });
    expect(wrapper.find('[data-testid="confirm-target"]').exists(), "the sheet stayed up").toBe(
      false,
    );
  });

  it("sends nothing when the operator cancels", async () => {
    const { wrapper, rig, call } = await asked("/thesis/chapter-3.md");

    const cancel = wrapper.findAll("button").find((b) => b.text() === "Cancel");
    await cancel!.trigger("click");

    expect(await call).toMatchObject({ status: "refused" });
    expect(rig.socket().frames().filter((f) => f.type === "confirm")).toEqual([]);
  });
});
