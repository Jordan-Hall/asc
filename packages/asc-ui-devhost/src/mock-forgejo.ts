// Mock Forgejo for the dev host and tests (SPEC §24.2, §24.7.1). It serves only the paths of the
// allowlist's `forgejo` section, shaped by the generated swagger types, so it cannot drift to
// endpoints the client would never call. Pure in-memory; no secrets are read or logged.
import type { ForgejoPaths } from "@asc/asc-client/forgejo";
import { Hono } from "hono";

type Json200<P extends keyof ForgejoPaths, M extends string> =
  ForgejoPaths[P] extends Record<
    M,
    { responses: { 200: { content: { "application/json": infer T } } } }
  >
    ? T
    : never;

export type InboxEntry = Json200<"/user/asc/inbox", "get">[number];
export type ActionRun = Json200<"/repos/{o}/{r}/actions/runs/{id}", "get">;

export interface MockRun {
  readonly run: ActionRun;
  readonly jobs: readonly { id: number; name: string; status: string }[];
  readonly logs: Readonly<Record<number, string>>;
}

export interface MockForgejoState {
  inbox: InboxEntry[];
  runs: MockRun[];
  /** `owner/repo`, the one repository the mock knows. */
  repo: string;
  merged: number[];
}

const pr = (number: number, title: string) => ({
  number,
  title,
  state: "open",
  draft: false,
  mergeable: true,
});

export function seedState(repo = "asc/demo"): MockForgejoState {
  return {
    repo,
    merged: [],
    inbox: [
      { id: 1, section: "needs_review", pinned: false, pull_request: pr(1, "Add stack view") },
      { id: 2, section: "ready", pinned: true, pull_request: pr(2, "Fix restack") },
    ],
    runs: [
      {
        run: { id: 10, title: "ci", status: "success", prettyref: "main" },
        jobs: [{ id: 100, name: "test", status: "success" }],
        logs: { 100: "ok\n" },
      },
    ],
  };
}

const notFound = { message: "not found" };

export function createMockForgejo(state: MockForgejoState = seedState()): Hono {
  const app = new Hono().basePath("/api/v1");

  app.get("/user", (c) => c.json({ id: 1, login: "demo", full_name: "Demo User" }));
  app.get("/user/asc/inbox", (c) => {
    const section = c.req.query("section");
    return c.json(section ? state.inbox.filter((e) => e.section === section) : state.inbox);
  });
  app.put("/user/asc/inbox/:id", async (c) => {
    const entry = state.inbox.find((e) => String(e.id) === c.req.param("id"));
    if (!entry) return c.json(notFound, 404);
    const body = (await c.req.json().catch(() => ({}))) as {
      pinned?: boolean;
      snoozed_until?: string;
    };
    if (typeof body.pinned === "boolean") entry.pinned = body.pinned;
    if (typeof body.snoozed_until === "string") entry.snoozed_until = body.snoozed_until;
    return c.json(entry);
  });

  const scoped = new Hono<{ Variables: Record<string, never> }>();
  scoped.use("*", async (c, next) => {
    if (`${c.req.param("o")}/${c.req.param("r")}` !== state.repo) return c.json(notFound, 404);
    await next();
  });
  scoped.get("/pulls/:n", (c) => {
    const hit = state.inbox.find((e) => String(e.pull_request?.number) === c.req.param("n"));
    return hit?.pull_request ? c.json(hit.pull_request) : c.json(notFound, 404);
  });
  scoped.get("/pulls/:n/reviews", (c) => c.json([]));
  scoped.post("/pulls/:n/merge", (c) => {
    const n = Number(c.req.param("n"));
    if (!state.inbox.some((e) => e.pull_request?.number === n)) return c.json(notFound, 404);
    state.merged.push(n);
    return c.body(null, 200);
  });
  scoped.get("/actions/runs", (c) =>
    c.json({ workflow_runs: state.runs.map((r) => r.run), total_count: state.runs.length }),
  );
  scoped.get("/actions/runs/:id", (c) => {
    const r = state.runs.find((x) => String(x.run?.id) === c.req.param("id"));
    return r ? c.json(r.run) : c.json(notFound, 404);
  });
  scoped.get("/actions/runs/:id/jobs", (c) => {
    const r = state.runs.find((x) => String(x.run?.id) === c.req.param("id"));
    return r ? c.json({ jobs: r.jobs, total_count: r.jobs.length }) : c.json(notFound, 404);
  });
  scoped.get("/actions/jobs/:id/logs", (c) => {
    const id = Number(c.req.param("id"));
    for (const r of state.runs) {
      const log = r.logs[id];
      if (log !== undefined) return c.text(log);
    }
    return c.json(notFound, 404);
  });
  app.route("/repos/:o/:r", scoped);
  app.all("*", (c) => c.json(notFound, 404));
  return app;
}
