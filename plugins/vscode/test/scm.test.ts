import { describe, expect, test } from "bun:test";
import { decorationFor } from "../src/decorations";
import {
  ByteLru,
  AscUriError,
  createAscReader,
  immutableKey,
  parseAscUri,
  type ReadApi,
  toAscUri,
} from "../src/fs/ascfs";
import {
  groupsOf,
  RESOURCE_CAP,
  type ScmApi,
  ScmModel,
  type StatusReply,
  type WatchMsg,
} from "../src/scm/provider";

const SHA = "a".repeat(40);

describe("scm: asc uris", () => {
  test("round trip for rev, stage and file", () => {
    for (const at of [
      { kind: "rev", rev: SHA },
      { kind: "stage", stage: 2 },
      { kind: "file", fileId: "deadbeef" },
    ] as const) {
      const u = toAscUri({ root: "C:/w r", path: "src/a b.ts", at });
      expect(parseAscUri(u.path, u.query)).toEqual({ root: "C:/w r", path: "src/a b.ts", at });
    }
  });
  test("rejects ambiguous, traversal and bad stage", () => {
    expect(() => parseAscUri("/a", "root=r")).toThrow(AscUriError);
    expect(() => parseAscUri("/a", "root=r&rev=x&stage=1")).toThrow(AscUriError);
    expect(() => parseAscUri("/../a", "root=r&rev=x")).toThrow(AscUriError);
    expect(() => parseAscUri("/a", "root=r&stage=4")).toThrow(AscUriError);
    expect(() => parseAscUri("/a", "rev=x")).toThrow(AscUriError);
  });
  test("only full commit ids and file ids are immutable", () => {
    const base = { root: "r", path: "a" };
    expect(immutableKey({ ...base, at: { kind: "rev", rev: SHA } })).not.toBeNull();
    expect(immutableKey({ ...base, at: { kind: "rev", rev: "HEAD" } })).toBeNull();
    expect(immutableKey({ ...base, at: { kind: "stage", stage: 0 } })).toBeNull();
    expect(immutableKey({ ...base, at: { kind: "file", fileId: "ab" } })).not.toBeNull();
  });
});

describe("scm: asc file reader", () => {
  const fake = () => {
    const calls: string[] = [];
    const api: ReadApi = {
      async readFile(_r, path) {
        calls.push(path);
        return new Uint8Array([1, 2, 3]);
      },
    };
    return { api, calls };
  };
  test("caches immutable content, not HEAD or stage", async () => {
    const { api, calls } = fake();
    const r = createAscReader(api);
    const imm = { root: "r", path: "a", at: { kind: "rev", rev: SHA } } as const;
    await Promise.all([r.read(imm), r.read(imm)]);
    await r.read(imm);
    expect(calls).toEqual(["a"]);
    const head = { root: "r", path: "b", at: { kind: "rev", rev: "HEAD" } } as const;
    await r.read(head);
    await r.read(head);
    expect(calls).toEqual(["a", "b", "b"]);
  });
  test("LRU evicts oldest past its byte cap and skips oversize values", () => {
    const c = new ByteLru(10);
    c.set("a", new Uint8Array(6));
    c.set("b", new Uint8Array(6));
    expect(c.get("a")).toBeUndefined();
    expect(c.get("b")).toBeDefined();
    c.set("big", new Uint8Array(11));
    expect(c.get("big")).toBeUndefined();
    expect(c.bytes).toBe(6);
  });
});

describe("scm: groups and decorations", () => {
  test("groupsOf", () => {
    expect(groupsOf("??")).toEqual(["untracked"]);
    expect(groupsOf("UU")).toEqual(["conflicts"]);
    expect(groupsOf("M.")).toEqual(["staged"]);
    expect(groupsOf(".M")).toEqual(["changes"]);
    expect(groupsOf("MM")).toEqual(["staged", "changes"]);
    expect(groupsOf("!!")).toEqual([]);
  });
  test("decorations use gitDecoration colours", () => {
    expect(decorationFor(".M")?.color).toBe("gitDecoration.modifiedResourceForeground");
    expect(decorationFor("??")?.badge).toBe("U");
    expect(decorationFor("UU")?.badge).toBe("!");
    expect(decorationFor("..")).toBeNull();
  });
});

function api(initial: StatusReply["files"]) {
  const state = { files: initial, requested: [] as (string[] | null)[], events: [] as WatchMsg[] };
  const a: ScmApi = {
    async readFile() {
      return new Uint8Array();
    },
    async status(_r, paths) {
      state.requested.push(paths);
      const files = paths ? state.files.filter((f) => paths.includes(f.path)) : state.files;
      return { headRef: "main", files, token: "t1", behind: 0 };
    },
    async *watch(_r, _t, signal) {
      for (const e of state.events) yield e;
      await new Promise((res) => signal.addEventListener("abort", res));
    },
    async stackParent() {
      return null;
    },
  };
  return { a, state };
}

describe("scm: model", () => {
  test("groups, count and targeted invalidation on a delta", async () => {
    const { a, state } = api([
      { path: "a.ts", xy: "MM" },
      { path: "b.ts", xy: "UU" },
      { path: "c.ts", xy: "??" },
    ]);
    const m = new ScmModel({ root: "r", api: a });
    const invalidated: string[][] = [];
    m.decorations.onDidChange((p) => invalidated.push(p));
    await m.refresh();
    const { groups } = m.groups();
    expect(groups.map((g) => [g.id, g.resources.length])).toEqual([
      ["conflicts", 1],
      ["staged", 1],
      ["changes", 1],
      ["untracked", 1],
    ]);
    expect(m.count()).toBe(2);
    invalidated.length = 0;
    state.files = [
      { path: "a.ts", xy: "MM" },
      { path: "b.ts", xy: "UU" },
      { path: "c.ts", xy: "??" },
      { path: "d.ts", xy: ".M" },
    ];
    await m.refresh(["d.ts", "a.ts"]);
    expect(invalidated).toEqual([["d.ts"]]);
    state.files = state.files.filter((f) => f.path !== "d.ts");
    await m.refresh(["d.ts"]);
    expect(m.xyFor("d.ts")).toBeUndefined();
    expect(invalidated.at(-1)).toEqual(["d.ts"]);
  });

  test("caps resources at 10,000 and reports the rest", async () => {
    const files = Array.from({ length: RESOURCE_CAP + 5 }, (_, i) => ({
      path: `f${String(i).padStart(6, "0")}`,
      xy: ".M",
    }));
    const { a } = api(files);
    const m = new ScmModel({ root: "r", api: a });
    await m.refresh();
    const { groups, more } = m.groups();
    expect(groups.reduce((n, g) => n + g.resources.length, 0)).toBe(RESOURCE_CAP);
    expect(more).toBe(5);
  });

  test("a watch paths event refreshes only those paths", async () => {
    const { a, state } = api([{ path: "a.ts", xy: ".M" }]);
    state.events = [{ kind: "paths", paths: ["a.ts"], token: "t2" }];
    const m = new ScmModel({ root: "r", api: a, debounceMs: 5 });
    m.start();
    await new Promise((r) => setTimeout(r, 80));
    m.stop();
    expect(state.requested[0]).toBeNull();
    expect(state.requested).toContainEqual(["a.ts"]);
  });
});
