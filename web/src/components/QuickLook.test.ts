// @vitest-environment happy-dom
import { describe, expect, it } from "vitest";
import { nextTick } from "vue";

import QuickLook from "./QuickLook.vue";
import { harness, mountInShell } from "../testing/shell-harness.ts";
import App from "../App.vue";

/**
 * The preview the operator actually sees.
 *
 * The store decides *which* viewer a file gets and is tested on its own; these
 * tests are about what reaches the screen, because this project has shipped
 * finished stores whose components showed something else. The two that matter
 * most are the honest refusal — a reason instead of a broken frame — and the
 * sandbox, which is the only thing between an HTML file in the working
 * directory and the operator's session.
 */

function openOn(path: string, options: { online?: boolean; siblings?: string[] } = {}) {
  const rig = harness({ online: options.online ?? true });
  const { wrapper } = mountInShell(QuickLook, { harness: rig });
  rig.shell.quicklook.open(path, options.siblings);
  return { rig, wrapper };
}

describe("before anything is asked for", () => {
  it("is not on the screen at all", () => {
    const { wrapper } = mountInShell(QuickLook);
    expect(wrapper.find("[role=dialog]").exists()).toBe(false);
  });
});

describe("a file a browser can decode", () => {
  it("is shown, from the node's content route", async () => {
    const { wrapper } = openOn("/Photos/holiday.jpg");
    await nextTick();

    const img = wrapper.find("img");
    expect(img.exists(), "an image was not rendered").toBe(true);
    expect(img.attributes("src")).toBe("/content?path=%2FPhotos%2Fholiday.jpg");
  });

  it("gets a player that can seek, for audio and video", async () => {
    for (const [path, tag] of [
      ["/clip.mp4", "video"],
      ["/song.mp3", "audio"],
    ] as const) {
      const { wrapper } = openOn(path);
      await nextTick();
      const player = wrapper.find(tag);
      expect(player.exists(), `${path} got no ${tag} element`).toBe(true);
      // Without controls there is no scrubber, which is the entire reason the
      // content route implements range requests.
      expect(player.attributes()).toHaveProperty("controls");
    }
  });

  it("announces itself as a dialog, because it takes the whole screen", async () => {
    const { wrapper } = openOn("/a.jpg");
    await nextTick();
    const dialog = wrapper.find("[role=dialog]");
    expect(dialog.exists()).toBe(true);
    expect(dialog.attributes("aria-modal")).toBe("true");
    expect(
      dialog.attributes("aria-label") ?? dialog.attributes("aria-labelledby"),
      "a screen reader would announce an unnamed dialog",
    ).toBeTruthy();
  });
});

describe("a file that can carry code", () => {
  it("is rendered in a frame that cannot script or reach this origin", async () => {
    // The sharpest edge in the product. An HTML file or an SVG in the working
    // directory could have arrived by download, email, sync or an agent.
    // Rendered same-origin it would run with the operator's session cookie.
    for (const path of ["/page.html", "/logo.svg"]) {
      const { wrapper } = openOn(path);
      await nextTick();

      const frame = wrapper.find("iframe");
      expect(frame.exists(), `${path} was not rendered in a frame at all`).toBe(true);

      const sandbox = frame.attributes("sandbox");
      expect(sandbox, `${path}'s frame is not sandboxed`).toBeDefined();
      expect(
        sandbox,
        `${path}'s sandbox grants script execution`,
      ).not.toContain("allow-scripts");
      expect(
        sandbox,
        `${path}'s sandbox puts the frame back in the shell's origin`,
      ).not.toContain("allow-same-origin");
    }
  });
});

describe("a file nothing here can show", () => {
  it("says why, rather than leaving a blank frame", async () => {
    // A camera raw is the case that matters: these are the operator's
    // photographs, and a broken <img> with no words is the worst answer.
    const { wrapper } = openOn("/DSC_0001.NEF");
    await nextTick();

    expect(wrapper.find("img").exists(), "a raw file was handed to an <img>").toBe(false);
    expect(wrapper.text()).toMatch(/raw/i);
    expect(wrapper.text().length, "the refusal said nothing").toBeGreaterThan(20);
  });

  it("still offers to hand the file over", async () => {
    // "I cannot show this" is only half an answer; the operator still wants
    // the file. The download is the escape hatch from every refusal.
    const { wrapper } = openOn("/archive.7z");
    await nextTick();

    const download = wrapper.find("a[download], a[href*='download=1']");
    expect(download.exists(), "a file that cannot be previewed cannot be fetched either").toBe(
      true,
    );
  });
});

describe("with no node", () => {
  it("explains the device is offline instead of showing an empty frame", async () => {
    const { wrapper } = openOn("/a.jpg", { online: false });
    await nextTick();

    expect(wrapper.find("img").exists(), "an image was requested with no network").toBe(false);
    expect(wrapper.text()).toMatch(/offline/i);
  });
});

describe("reviewing a folder", () => {
  it("moves to the next file and says where it is", async () => {
    const siblings = ["/a.jpg", "/b.png", "/c.png"];
    const { rig, wrapper } = openOn("/a.jpg", { siblings });
    await nextTick();

    rig.shell.quicklook.next();
    await nextTick();

    expect(wrapper.find("img").attributes("src")).toBe("/content?path=%2Fb.png");
    expect(wrapper.text(), "no sense of position in the folder").toMatch(/2.*3/);
  });
});

describe("closing it", () => {
  it("is possible without a keyboard", async () => {
    const { rig, wrapper } = openOn("/a.jpg");
    await nextTick();

    const close = wrapper.find("button[aria-label*='lose'], button[aria-label*='ismiss']");
    expect(close.exists(), "a phone has no Escape key").toBe(true);

    await close.trigger("click");
    await nextTick();

    expect(rig.shell.quicklook.visible.value).toBe(false);
    expect(wrapper.find("[role=dialog]").exists()).toBe(false);
  });
});

describe("the shell itself", () => {
  it("includes the preview, so it can ever be reached at all", () => {
    // The construction guard. This project has repeatedly built a module
    // complete, unit-tested it in isolation, and never constructed it: tests
    // green, feature impossible at runtime. Every test above mounts QuickLook
    // directly and would stay green if App.vue stopped including it.
    //
    // Shallow, and asserting the component rather than the rendered dialog:
    // a full mount of App drags in the terminal and every panel, and what is
    // being checked here is composition, not rendering -- the rendering is
    // what the rest of this file is for.
    const rig = harness();
    const { wrapper } = mountInShell(App, {
      harness: rig,
      props: { shell: rig.shell },
      shallow: true,
    });

    expect(
      wrapper.findComponent(QuickLook).exists(),
      "the shell does not mount QuickLook, so nothing can ever be previewed",
    ).toBe(true);
  });
});
