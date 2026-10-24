import { describe, expect, test } from "bun:test";
import {
  bannerFor,
  type ConflictsApi,
  type ConflictsState,
  ConflictsModel,
  hasConflictMarkers,
  mergeEditorArgs,
  planOpen,
} from "../src/conflicts";

const uriOf = (p: string, at: { kind: string; stage?: number }) =>
  `asc:/${p}?${at.kind}=${at.stage ?? ""}`;
const entry = (path: string) => ({ path, marked: new Uint8Array() });

function fake(states: ConflictsState[], exit = 0) {
  const calls: string[] = [];
  let i = 0;
  const api: ConflictsApi = {
    async load() {
      return states[Math.min(i++, states.length - 1)] as ConflictsState;
    },
    async stageAdd(_r, p) {
      calls.push(`add:${p}`);
    },
    async run(_r, verb) {
      calls.push(`run:${verb}`);
      return exit;
    },
  };
  return { api, calls };
}

describe("conflicts: banner", () => {
  test("restack with conflicts", () => {
    const b = bannerFor({ inProgress: "rebase", conflicts: [entry("a"), entry("b")] });
    expect(b?.text).toBe("Restack paused: 2 conflicts · Continue · Abort");
  });
  test("singular and none", () => {
    expect(bannerFor({ inProgress: "merge", conflicts: [entry("a")] })?.text).toBe(
      "Merge paused: 1 conflict · Continue · Abort",
    );
    expect(bannerFor({ inProgress: "", conflicts: [] })).toBeNull();
    expect(bannerFor({ inProgress: "rebase", conflicts: [] })?.text).toContain("paused");
  });
});

describe("conflicts: markers", () => {
  test("detects diff3 markers", () => {
    expect(hasConflictMarkers("a\n<<<<<<< ours\nx\n||||||| base\ny\n=======\nz\n>>>>>>> theirs\n")).toBe(true);
    expect(hasConflictMarkers("a\n=======\nb\n")).toBe(false);
    expect(hasConflictMarkers("x <<<<<<< y")).toBe(false);
    expect(hasConflictMarkers("clean\n")).toBe(false);
  });
});

describe("conflicts: open plan", () => {
  test("merge editor uses stages 1 to 3 and swaps during a restack", () => {
    const m = mergeEditorArgs(uriOf as never, "file:///w/a", "a", "merge");
    expect(m.base).toContain("stage=1");
    expect(m.input1.uri).toContain("stage=2");
    expect(m.input2.uri).toContain("stage=3");
    const r = mergeEditorArgs(uriOf as never, "file:///w/a", "a", "rebase");
    expect(r.input1.uri).toContain("stage=3");
    expect(r.input2.uri).toContain("stage=2");
    expect(r.output).toBe("file:///w/a");
  });
  test("falls back to markers when _open.mergeEditor is absent", () => {
    expect(planOpen(false, uriOf as never, "f", entry("a"), "merge")).toEqual({
      kind: "markers",
      path: "a",
    });
    expect(planOpen(true, uriOf as never, "f", entry("a"), "merge").kind).toBe("mergeEditor");
  });
});

describe("conflicts: model", () => {
  test("mark resolved refuses while markers remain, then stages and refreshes", async () => {
    const { api, calls } = fake([
      { inProgress: "rebase", conflicts: [entry("a")] },
      { inProgress: "rebase", conflicts: [] },
    ]);
    const m = new ConflictsModel("/w", api);
    await m.refresh();
    expect(await m.markResolved("a", "<<<<<<< x\n")).toBe(false);
    expect(calls).toEqual([]);
    expect(await m.markResolved("a", "done\n")).toBe(true);
    expect(calls).toEqual(["add:a"]);
    expect(m.state.conflicts).toHaveLength(0);
  });
  test("continue and abort run the verbs", async () => {
    const { api, calls } = fake([{ inProgress: "", conflicts: [] }]);
    const m = new ConflictsModel("/w", api);
    await m.finish("continue");
    await m.finish("abort");
    expect(calls).toEqual(["run:continue", "run:abort"]);
  });
  test("continue that stops on new conflicts is not an error; other failures throw", async () => {
    const stop = fake([{ inProgress: "rebase", conflicts: [entry("b")] }], 1);
    await new ConflictsModel("/w", stop.api).finish("continue");
    const bad = fake([{ inProgress: "", conflicts: [] }], 2);
    await expect(new ConflictsModel("/w", bad.api).finish("continue")).rejects.toThrow("exited with 2");
    await expect(new ConflictsModel("/w", bad.api).finish("abort")).rejects.toThrow("exited with 2");
  });
});
