import { describe, expect, test } from "bun:test";
import { create, toBinary } from "@bufbuild/protobuf";
import { RunRequestSchema } from "@asc/asc-client/bridge";
import { graphConfirm, parseGraphRun, withGraphActions } from "../src/webview/graph-actions";
import type { WebviewLike } from "../src/webview/host";

const REV = "a1b2c3d4".repeat(5);
const run = (verb: string, args: string[], id = 1) => ({
  id,
  method: "asc.v1.Bridge/Run",
  bytes: toBinary(RunRequestSchema, create(RunRequestSchema, { verb, args })),
});

function fixture(answer: boolean | "throw") {
  const sent: unknown[] = [];
  const forwarded: unknown[] = [];
  const asked: { title: string; targets: string[] }[] = [];
  const w = { postMessage: async (m: unknown) => (sent.push(m), true) } as unknown as WebviewLike;
  const handler = withGraphActions({
    inner: (_w, m) => (forwarded.push(m), true),
    confirm: async (c) => {
      asked.push({ title: c.title, targets: c.targets });
      if (answer === "throw") throw new Error("x");
      return answer;
    },
  });
  return { w, handler, sent, forwarded, asked };
}

describe("graph-actions", () => {
  test("confirmed cherry-pick and revert are forwarded after a host-composed prompt", async () => {
    for (const verb of ["cherry-pick", "revert"]) {
      const f = fixture(true);
      const m = run(verb, [REV]);
      expect(await f.handler(f.w, m)).toBe(true);
      expect(f.forwarded).toEqual([m]);
      expect(f.asked[0]?.title).toContain(REV.slice(0, 8));
      expect(f.asked[0]?.targets).toEqual([REV]);
    }
  });

  test("a refusal or a failing modal never reaches the relay", async () => {
    for (const a of [false, "throw"] as const) {
      const f = fixture(a);
      expect(await f.handler(f.w, run("revert", [REV], 9))).toBe(true);
      expect(f.forwarded).toEqual([]);
      expect(f.sent).toEqual([{ id: 9, end: { code: 7, message: "not confirmed" } }]);
    }
  });

  test("flags, short ids, several revs and free text are not taken as graph calls", async () => {
    for (const args of [["-n", REV], ["HEAD"], ["abc123"], [REV, REV], [`${REV}\nx`], [REV.toUpperCase()]]) {
      expect(parseGraphRun(run("cherry-pick", args))).toBeUndefined();
      const f = fixture(true);
      await f.handler(f.w, run("cherry-pick", args));
      expect(f.asked).toEqual([]);
      expect(f.forwarded.length).toBe(1); // handed to the relay, whose allowlist decides
    }
  });

  test("other verbs and other messages pass straight to the relay without a prompt", async () => {
    const f = fixture(false);
    await f.handler(f.w, run("status", []));
    await f.handler(f.w, "junk");
    expect(f.asked).toEqual([]);
    expect(f.forwarded.length).toBe(2);
  });

  test("the confirmation text ignores anything the webview could add", () => {
    const c = graphConfirm({ verb: "revert", rev: REV });
    expect(c.title).toBe(`Revert ${REV.slice(0, 8)} on the current checkout?`);
  });
});
