import { describe, expect, test } from "bun:test";
import { create, fromBinary, toBinary } from "@bufbuild/protobuf";
import type { BridgeNode } from "@asc/asc-client";
import {
  CiStatus,
  JobLogRequestSchema,
  JobLogResponseSchema,
  ListRunsResponseSchema,
} from "@asc/asc-client/ci";
import type { BridgeManager } from "../src/bridge";
import {
  ActionsLogDocs,
  ActionsLogModel,
  ActionsUriError,
  CHUNK_BYTES,
  type LogRange,
  type LogRangeRequest,
  type LogReadApi,
  parseActionsUri,
  sanitizeLog,
  TAIL_BYTES,
  toActionsUri,
} from "../src/fs/actions-log";
import { buildTaskSpecs, projectCwd } from "../src/tasks";
import {
  type ActionsApi,
  ActionsModel,
  bridgeActionsApi,
  describeRun,
  iconFor,
  type RunRow,
  splitRepo,
} from "../src/views/actions";
import {
  type AffectedApi,
  AffectedModel,
  cleanTarget,
  describeAffected,
  equivalentCommand,
} from "../src/views/affected";

const enc = new TextEncoder();
const sleep = (ms: number) => new Promise((r) => setTimeout(r, ms));

/** A log served by range, like the forge does. */
function logApi(data: Uint8Array): LogReadApi & { calls: LogRangeRequest[] } {
  const calls: LogRangeRequest[] = [];
  return {
    calls,
    async readRange(_id, r): Promise<LogRange> {
      calls.push(r);
      const total = data.length;
      const start = "suffix" in r ? Math.max(0, total - r.suffix) : r.start;
      const end = "suffix" in r ? total : Math.min(total, r.end + 1);
      return { bytes: data.subarray(start, end), start, total };
    },
  };
}

describe("ci: asc-actions log documents", () => {
  test("uri round trip and rejection", () => {
    const u = toActionsUri({ owner: "asc", repo: "asc", jobId: 42 });
    expect(u).toEqual({ scheme: "asc-actions", path: "/asc/asc/job/42.log" });
    expect(parseActionsUri(u.path)).toEqual({ owner: "asc", repo: "asc", jobId: 42 });
    expect(() => parseActionsUri("/a/b/job/x.log")).toThrow(ActionsUriError);
    expect(() => parseActionsUri("/a/../job/1.log")).toThrow(ActionsUriError);
    expect(() => toActionsUri({ owner: "a/b", repo: "r", jobId: 1 })).toThrow(ActionsUriError);
    expect(() => toActionsUri({ owner: "a", repo: "r", jobId: -1 })).toThrow(ActionsUriError);
  });

  test("a short log is read whole with one suffix range", async () => {
    const api = logApi(enc.encode("line 1\nline 2\n"));
    const m = new ActionsLogModel(1, api);
    await m.loadTail();
    expect(m.text()).toBe("line 1\nline 2\n");
    expect(m.hasEarlier).toBe(false);
    expect(api.calls).toEqual([{ suffix: TAIL_BYTES }]);
  });

  test("tail 64 KiB then load earlier grows backwards without losing bytes", async () => {
    const lines = Array.from({ length: 20000 }, (_, i) => `row ${i}\n`).join("");
    const data = enc.encode(lines);
    expect(data.length).toBeGreaterThan(2 * CHUNK_BYTES);
    const api = logApi(data);
    const m = new ActionsLogModel(7, api);
    await m.loadTail();
    expect(m.hasEarlier).toBe(true);
    expect(m.text()).toContain("earlier bytes not shown");
    expect(m.text().endsWith("row 19999\n")).toBe(true);
    await m.loadEarlier();
    expect(api.calls[1]).toEqual({
      start: data.length - 2 * CHUNK_BYTES,
      end: data.length - CHUNK_BYTES - 1,
    });
    while (m.hasEarlier) await m.loadEarlier();
    expect(m.text()).toBe(lines);
  });

  test("a range that starts inside a multibyte character drops the partial bytes", async () => {
    const data = enc.encode(`${"é".repeat(TAIL_BYTES)}\nend\n`);
    const m = new ActionsLogModel(1, logApi(data));
    await m.loadTail();
    expect(m.text()).not.toContain("\uFFFD");
  });

  test("terminal escapes and control characters never reach the document", () => {
    expect(sanitizeLog("a\u001b[31mred\u001b[0m\u001b]0;title\u0007b\u0000c\r\nd\te")).toBe(
      "aredbc\nd\te",
    );
  });

  test("docs reuse the loaded model and refuse an unknown repository", async () => {
    const api = logApi(enc.encode("hello\n"));
    const docs = new ActionsLogDocs((o, r) => (o === "asc" && r === "asc" ? api : undefined));
    expect(await docs.provide("/asc/asc/job/3.log")).toBe("hello\n");
    expect(await docs.provide("/asc/asc/job/3.log")).toBe("hello\n");
    expect(api.calls.length).toBe(1);
    expect(docs.provide("/other/asc/job/3.log")).rejects.toThrow(ActionsUriError);
  });
});

describe("ci: Actions model", () => {
  const run = (o: Partial<RunRow> = {}): RunRow => ({
    id: 1,
    title: "CI",
    status: "success",
    source: "runner",
    version: 2,
    jobs: [{ id: 11, runId: 1, name: "j", status: "success" }],
    ...o,
  });
  const target = async () => ({ owner: "asc", repo: "asc", change: "k".repeat(32) });
  const mk = (runs: RunRow[], calls: { n: number; changes: (string | undefined)[] }) => {
    const api = {
      listRuns: async (change) => (calls.n++, calls.changes.push(change), runs),
      readRange: async () => ({ bytes: new Uint8Array(0), start: 0, total: 0 }),
    } satisfies ActionsApi;
    return new ActionsModel({ api, target });
  };

  test("fetches only while visible and refreshes stale on show", async () => {
    const calls = { n: 0, changes: [] as (string | undefined)[] };
    const m = mk([run()], calls);
    m.markStale();
    await sleep(5);
    expect(calls.n).toBe(0);
    m.setVisible(true);
    await sleep(5);
    expect(calls.n).toBe(1);
    expect(m.runs.length).toBe(1);
    expect(m.current?.owner).toBe("asc");
    expect(calls.changes).toEqual(["k".repeat(32)]);
    m.markStale();
    await sleep(5);
    expect(calls.n).toBe(2);
  });

  test("a root without an asc server shows no runs and never calls the api", async () => {
    const calls = { n: 0, changes: [] as (string | undefined)[] };
    const m = new ActionsModel({
      api: mk([run()], calls) as never,
      target: async () => undefined,
    });
    await m.refresh();
    expect(calls.n).toBe(0);
    expect(m.runs).toEqual([]);
  });

  test("a run row names its status, source and version", () => {
    expect(describeRun(run()).description).toBe("success  runner  v2");
    expect(describeRun(run({ title: "", version: 0 })).label).toBe("Run 1");
    expect(iconFor("failure")).toBe("error");
    expect(iconFor("queued")).toBe("clock");
  });

  test("an error is reported and keeps the old rows", async () => {
    let fail = false;
    const errs: unknown[] = [];
    const m = new ActionsModel({
      api: {
        listRuns: async () => {
          if (fail) throw new Error("boom");
          return [run()];
        },
        readRange: async () => ({ bytes: new Uint8Array(0), start: 0, total: 0 }),
      },
      target: async () => ({ owner: "o", repo: "r" }),
      onError: (e) => errs.push(e),
    });
    await m.refresh();
    fail = true;
    await m.refresh();
    expect(errs.length).toBe(1);
    expect(m.runs.length).toBe(1);
  });
});

describe("ci: bridge adapter", () => {
  function fake(log: Uint8Array) {
    const seen: string[] = [];
    const node: BridgeNode = {
      state: "up",
      pid: undefined,
      stderrTail: () => [],
      close() {},
      stream: async function* () {},
      unary: async (o) => {
        if (o.method === "asc.v1.Ci/ListRuns") {
          seen.push("runs");
          return toBinary(
            ListRunsResponseSchema,
            create(ListRunsResponseSchema, {
              runs: [
                {
                  id: 5n,
                  name: "build",
                  source: "runner",
                  version: 3,
                  status: CiStatus.FAILURE,
                  jobs: [{ id: 9n, name: "unit", status: CiStatus.RUNNING }],
                },
              ],
            }),
          );
        }
        const req = fromBinary(JobLogRequestSchema, o.request);
        seen.push(`log:${req.runId}:${req.jobId}:${req.maxBytes}:${req.sinceOffset}`);
        // The server cuts the tail to maxBytes, or answers from sinceOffset on.
        const from =
          req.sinceOffset > 0n ? Number(req.sinceOffset) : Math.max(0, log.length - req.maxBytes);
        const data = log.subarray(from, Math.min(log.length, from + req.maxBytes));
        return toBinary(
          JobLogResponseSchema,
          create(JobLogResponseSchema, {
            data,
            offset: BigInt(from),
            nextOffset: BigInt(from + data.length),
            size: BigInt(log.length),
          }),
        );
      },
    };
    return { seen, mgr: { node: () => node } as unknown as BridgeManager };
  }
  const t = { root: "/w", repo: "o/r" };
  const log = new TextEncoder().encode("0123456789");

  test("runs map status, source and jobs; the log needs the run listed first", async () => {
    const f = fake(log);
    const api = bridgeActionsApi(f.mgr, t);
    await expect(api.readRange(9, { suffix: 4 })).rejects.toThrow("not loaded");
    const runs = await api.listRuns(undefined);
    expect(runs).toEqual([
      {
        id: 5,
        title: "build",
        status: "failure",
        source: "runner",
        version: 3,
        jobs: [{ id: 9, runId: 5, name: "unit", status: "running" }],
      },
    ]);
    const tail = await api.readRange(9, { suffix: 4 });
    expect([new TextDecoder().decode(tail.bytes), tail.start, tail.total]).toEqual(["6789", 6, 10]);
    expect(f.seen).toContain("log:5:9:4:0");
  });

  test("a range reads from its offset; the first bytes read the whole log once", async () => {
    const f = fake(log);
    const api = bridgeActionsApi(f.mgr, t);
    await api.listRuns(undefined);
    const mid = await api.readRange(9, { start: 2, end: 5 });
    expect([new TextDecoder().decode(mid.bytes), mid.start]).toEqual(["2345", 2]);
    const head = await api.readRange(9, { start: 0, end: 2 });
    expect([new TextDecoder().decode(head.bytes), head.start, head.total]).toEqual(["012", 0, 10]);
  });

  test("splitRepo needs owner/repo", () => {
    expect(splitRepo("asc/asc")).toEqual({ owner: "asc", repo: "asc" });
    expect(splitRepo("asc")).toBeUndefined();
    expect(splitRepo("/asc")).toBeUndefined();
  });
});

describe("ci: Affected and Run target", () => {
  test("target names and argv are validated", () => {
    expect(cleanTarget("test", ["bun", "run", "test"])).toEqual({
      name: "test",
      argv: ["bun", "run", "test"],
    });
    expect(cleanTarget("bad name", ["x"])).toBeUndefined();
    expect(cleanTarget("--x", ["x"])).toBeUndefined();
    expect(cleanTarget("t", [])).toBeUndefined();
    expect(cleanTarget("t", [""])).toBeUndefined();
    expect(cleanTarget("t", ["a\0b"])).toBeUndefined();
  });

  test("the tree row names the reason and the equivalent asc command", () => {
    const d = describeAffected({
      id: "p",
      name: "web",
      root: "apps/web",
      direct: false,
      reasons: ["dep: core"],
      targets: [],
    });
    expect(d.label).toBe("web");
    expect(d.description).toContain("depends on a changed project");
    expect(d.tooltip).toContain("apps/web");
    expect(equivalentCommand("test")).toBe("asc affected --target test");
  });

  test("task specs: one per project and target, none without a definition, cwd stays inside", () => {
    const specs = buildTaskSpecs("D:\\work\\repo", [
      {
        id: "a",
        name: "alpha",
        root: "packages/alpha",
        targets: [
          { name: "test", argv: ["bun", "run", "test"] },
          { name: "lint", argv: ["oxlint"] },
        ],
      },
      { id: "b", name: "beta", root: "packages/beta", targets: [] },
      { id: "c", name: "evil", root: "../outside", targets: [{ name: "t", argv: ["x"] }] },
      { id: "d", name: "abs", root: "C:/Windows", targets: [{ name: "t", argv: ["x"] }] },
    ]);
    expect(specs.map((s) => `${s.projectName}:${s.target}`)).toEqual(["alpha:test", "alpha:lint"]);
    expect(specs[0]?.command).toBe("bun");
    expect(specs[0]?.args).toEqual(["run", "test"]);
    expect(specs[0]?.cwd).toBe("D:\\work\\repo\\packages\\alpha");
    expect(specs[0]?.detail).toContain("asc affected --target test");
  });

  test("cwd rejects escapes and accepts the root", () => {
    expect(projectCwd("/w", "")).toBe("/w");
    expect(projectCwd("/w", ".")).toBe("/w");
    expect(projectCwd("/w", "a/b")).toBe("/w/a/b");
    expect(projectCwd("/w", "a/../../b")).toBeUndefined();
    expect(projectCwd("/w", "/etc")).toBeUndefined();
    expect(projectCwd("/w", "a\\..\\..\\b")).toBeUndefined();
  });

  test("the model fetches only while visible; current() answers for the task provider", async () => {
    let n = 0;
    const api: AffectedApi = {
      affected: async () => (
        n++,
        {
          all: false,
          projects: [{ id: "a", name: "a", root: "a", direct: true, reasons: [], targets: [] }],
        }
      ),
    };
    const m = new AffectedModel({ root: "/w", api });
    m.onWatch();
    await sleep(5);
    expect(n).toBe(0);
    expect((await m.current()).projects.length).toBe(1);
    expect(n).toBe(1);
    await m.current();
    expect(n).toBe(1);
    m.setVisible(true);
    expect(n).toBe(1);
  });
});
