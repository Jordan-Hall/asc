import { describe, expect, test } from "bun:test";
import { LiveModel, liveView, parseLiveStatus, type LiveApi } from "../src/stack/live";

const enc = (v: unknown) => new TextEncoder().encode(JSON.stringify(v));

function setup(status: unknown) {
  const runs: { verb: string; args: string[] }[] = [];
  let current = status;
  const api: LiveApi = {
    run: async (_r, verb, args) => {
      runs.push({ verb, args });
      if (verb === "live") current = { paused: args[0] === "pause", pauseReason: args[0] === "pause" ? "user" : null };
      return { exit: 0 };
    },
    status: async () => enc(current),
  };
  return { m: new LiveModel({ api, root: "/w" }), runs };
}

describe("live: restack item", () => {
  test("shows trunk moved N commits - Restack", () => {
    expect(liveView(3, null).restack.text).toBe("trunk moved 3 commits - Restack");
    expect(liveView(1, null).restack.text).toBe("trunk moved 1 commit - Restack");
  });
  test("no item at N = 0, and junk N is clamped", () => {
    expect(liveView(0, null).restack.visible).toBe(false);
    expect(liveView(-5, null).restack.visible).toBe(false);
    expect(liveView(Number.NaN, null).restack.visible).toBe(false);
    expect(liveView(1e9, null).restack.text).toBe("trunk moved 999 commits - Restack");
  });
  test("restack runs the restack verb and nothing else", async () => {
    const s = setup({ paused: false });
    expect(await s.m.restack()).toBe(true);
    expect(s.runs).toEqual([{ verb: "restack", args: [] }]);
  });
});

describe("live: toggle", () => {
  test("calls the matching verb", async () => {
    const s = setup({ paused: false });
    await s.m.refresh();
    expect(s.m.view().toggle.verb).toBe("pause");
    await s.m.toggle();
    expect(s.runs.at(-1)).toEqual({ verb: "live", args: ["pause"] });
    expect(s.m.view().toggle.verb).toBe("resume");
    await s.m.toggle();
    expect(s.runs.at(-1)).toEqual({ verb: "live", args: ["resume"] });
    expect(s.runs.every((r) => r.verb === "live")).toBe(true);
  });
});

describe("live: conflict notice", () => {
  test("appears on paused-by-conflict and clears on resume", async () => {
    const s = setup({ paused: true, pauseReason: "conflict" });
    await s.m.refresh();
    expect(s.m.view().notice.visible).toBe(true);
    expect(s.m.view().toggle.verb).toBe("resume");
    await s.m.toggle();
    expect(s.m.view().notice.visible).toBe(false);
  });
  test("a user pause shows no notice", () => {
    expect(liveView(0, { paused: true, reason: "user" }).notice.visible).toBe(false);
  });
  test("untrusted output: junk rejected, no text shown", () => {
    for (const bad of [null, new Uint8Array(), new TextEncoder().encode("{"), enc([]), enc({ paused: "yes" })]) {
      expect(parseLiveStatus(bad)).toBeNull();
    }
    const v = liveView(0, parseLiveStatus(enc({ paused: true, pauseReason: "conflict", detail: "<script>" })));
    expect(v.notice.text).not.toContain("script");
  });
});
