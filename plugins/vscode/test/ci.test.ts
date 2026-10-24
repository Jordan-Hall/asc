import { describe, expect, test } from "bun:test";
import { createForgejoClient } from "@asc/asc-client/forgejo";
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
  forgejoActionsApi,
  parseContentRange,
  rerunDisabledReason,
  type RunRow,
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

describe("ci: Forgejo Actions adapter", () => {
  const seen: { url: string; method: string; range: string | null; auth: string | null }[] = [];
  const json = (b: unknown, status = 200, h: Record<string, string> = {}) =>
    new Response(JSON.stringify(b), {
      status,
      headers: { "content-type": "application/json", ...h },
    });
  const client = (fetchImpl: (u: URL, r: Request) => Response) =>
    createForgejoClient({
      baseUrl: "https://forge.example",
      getToken: () => "tok-secret",
      repos: ["asc/asc"],
      fetch: async (req) => {
        seen.push({
          url: req.url,
          method: req.method,
          range: req.headers.get("range"),
          auth: req.headers.get("authorization"),
        });
        return fetchImpl(new URL(req.url), req);
      },
    });

  test("lists runs filtered by head sha and jobs", async () => {
    seen.length = 0;
    const c = client((u) =>
      u.pathname.endsWith("/jobs")
        ? json([{ id: 9, name: "build", status: "success" }])
        : json({
            workflow_runs: [
              { id: 5, title: "CI", status: "failure", event: "push", commit_sha: "abcdef0123456789" },
            ],
          }),
    );
    const api = forgejoActionsApi(c, "asc", "asc");
    const runs = await api.listRuns("abcdef0123456789");
    expect(runs).toEqual([
      { id: 5, title: "CI", status: "failure", event: "push", commit: "abcdef0123456789" },
    ]);
    expect(seen[0]?.url).toContain("head_sha=abcdef0123456789");
    expect(seen[0]?.auth).toBe("token tok-secret");
    expect(await api.listJobs(5)).toEqual([{ id: 9, runId: 5, name: "build", status: "success" }]);
  });

  test("reads a log tail with a suffix Range and a 206 Content-Range", async () => {
    seen.length = 0;
    const body = enc.encode("tail text\n");
    const c = client(
      () =>
        new Response(body, {
          status: 206,
          headers: { "content-range": `bytes 90-99/100`, "content-type": "text/plain" },
        }),
    );
    const r = await forgejoActionsApi(c, "asc", "asc").readRange(9, { suffix: 64 });
    expect(seen[0]?.range).toBe("bytes=-64");
    expect(r.start).toBe(90);
    expect(r.total).toBe(100);
    expect(new TextDecoder().decode(r.bytes)).toBe("tail text\n");
  });

  test("a server that ignores Range is cut client side", async () => {
    const c = client(() => new Response(enc.encode("0123456789"), { status: 200 }));
    const api = forgejoActionsApi(c, "asc", "asc");
    const t = await api.readRange(1, { suffix: 4 });
    expect([t.start, t.total, new TextDecoder().decode(t.bytes)]).toEqual([6, 10, "6789"]);
    const e = await api.readRange(1, { start: 2, end: 4 });
    expect([e.start, new TextDecoder().decode(e.bytes)]).toEqual([2, "234"]);
  });

  test("a repository outside the opened roots is denied", async () => {
    const c = client(() => json({}));
    expect(forgejoActionsApi(c, "evil", "repo").listRuns(undefined)).rejects.toThrow();
  });

  test("content-range parsing", () => {
    expect(parseContentRange("bytes 0-9/10")).toEqual({ start: 0, total: 10 });
    expect(parseContentRange("bytes */10")).toBeUndefined();
    expect(parseContentRange(null)).toBeUndefined();
  });
});

describe("ci: Actions model", () => {
  const run = (o: Partial<RunRow> = {}): RunRow => ({
    id: 1,
    title: "CI",
    status: "success",
    event: "push",
    commit: "abc",
    ...o,
  });
  const mk = (runs: RunRow[], calls: { n: number; rerun: number[] }) => {
    const api = {
      listRuns: async () => (calls.n++, runs),
      listJobs: async (id: number) => [{ id: 10 + id, runId: id, name: "j", status: "success" }],
      rerun: async (id: number) => void calls.rerun.push(id),
      readRange: async () => ({ bytes: new Uint8Array(0), start: 0, total: 0 }),
    } satisfies ActionsApi;
    return new ActionsModel({ api, target: () => ({ owner: "asc", repo: "asc", headSha: "abc" }) });
  };

  test("fetches only while visible and refreshes stale on show", async () => {
    const calls = { n: 0, rerun: [] as number[] };
    const m = mk([run()], calls);
    m.markStale();
    await sleep(5);
    expect(calls.n).toBe(0);
    m.setVisible(true);
    await sleep(5);
    expect(calls.n).toBe(1);
    expect(m.runs.length).toBe(1);
    m.markStale();
    await sleep(5);
    expect(calls.n).toBe(2);
  });

  test("jobs load once per run", async () => {
    const m = mk([run()], { n: 0, rerun: [] });
    const a = await m.loadJobs(1);
    expect(await m.loadJobs(1)).toBe(a);
  });

  test("re-run is refused for an unfinished run and refreshes after a finished one", async () => {
    const calls = { n: 0, rerun: [] as number[] };
    const m = mk([run()], calls);
    m.setVisible(true);
    await expect(m.rerun(run({ status: "running" }))).rejects.toThrow("not finished");
    expect(calls.rerun).toEqual([]);
    expect(rerunDisabledReason("queued")).toBeDefined();
    await m.rerun(run({ id: 4 }));
    expect(calls.rerun).toEqual([4]);
    await sleep(5);
    expect(calls.n).toBe(2);
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
        listJobs: async () => [],
        rerun: async () => {},
        readRange: async () => ({ bytes: new Uint8Array(0), start: 0, total: 0 }),
      },
      target: () => ({ owner: "o", repo: "r" }),
      onError: (e) => errs.push(e),
    });
    await m.refresh();
    fail = true;
    await m.refresh();
    expect(errs.length).toBe(1);
    expect(m.runs.length).toBe(1);
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
        { all: false, projects: [{ id: "a", name: "a", root: "a", direct: true, reasons: [], targets: [] }] }
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
