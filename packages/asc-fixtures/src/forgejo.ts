// The forge side of the `review` scenario: pull requests, reviews, inbox entries and Actions runs
// for the changes of the `stacks` scenario, and a Hono mock Forgejo that serves them (SPEC section
// 24.7.2). Everything is a pure function of the stacks, with fixed times, so a build is
// repeatable. Pure in memory: no network, no secrets.
import { Hono } from "hono";
import { EPOCH } from "./asc";

export interface MockPull {
  number: number;
  title: string;
  state: "open" | "closed";
  draft: boolean;
  mergeable: boolean;
  /** `refs/for/<trunk>/asc-<change>` is what `asc submit` pushes; this is the head branch it makes. */
  head: { ref: string };
  base: { ref: string };
  /** The 12-letter change id the PR carries. */
  change: string;
  created_at: string;
  updated_at: string;
}

export interface MockReview {
  id: number;
  user: { login: string };
  state: "APPROVED" | "REQUEST_CHANGES" | "COMMENT";
  body: string;
  submitted_at: string;
}

export interface MockInboxEntry {
  id: number;
  section: "needs_review" | "ready" | "waiting";
  pinned: boolean;
  snoozed_until?: string;
  pull_request: MockPull;
}

export interface MockRun {
  id: number;
  title: string;
  status: "success" | "failure" | "running";
  prettyref: string;
  pull: number;
  jobs: { id: number; name: string; status: string }[];
  logs: Record<number, string>;
}

export interface ReviewData {
  repo: string;
  user: { id: number; login: string; full_name: string };
  pulls: MockPull[];
  reviews: Record<number, MockReview[]>;
  inbox: MockInboxEntry[];
  runs: MockRun[];
  merged: number[];
}

export interface StackChange {
  readonly change: string;
  readonly subject: string;
}

const iso = (offset: number) => new Date((EPOCH + offset) * 1000).toISOString();

/**
 * Review data for `changes` (bottom to top, stacks one after the other). Pull request `n` belongs
 * to the `n`th change; the first of each stack is reviewed, the second asks for changes, runs
 * cycle through success, failure and running.
 */
export function reviewDataFor(changes: readonly StackChange[], repo = "asc/demo"): ReviewData {
  const pulls: MockPull[] = changes.map((c, i) => ({
    number: i + 1,
    title: c.subject,
    state: "open",
    draft: i % 4 === 3,
    mergeable: i % 5 !== 4,
    head: { ref: `asc-${c.change}` },
    base: { ref: "master" },
    change: c.change,
    created_at: iso(100 + i),
    updated_at: iso(200 + i),
  }));
  const reviews: Record<number, MockReview[]> = {};
  pulls.forEach((p, i) => {
    reviews[p.number] = [];
    if (i % 3 === 0) {
      reviews[p.number]?.push({
        id: 1000 + i,
        user: { login: "reviewer-one" },
        state: "APPROVED",
        body: "Looks good.",
        submitted_at: iso(300 + i),
      });
    } else if (i % 3 === 1) {
      reviews[p.number]?.push({
        id: 1000 + i,
        user: { login: "reviewer-two" },
        state: "REQUEST_CHANGES",
        body: "Please add a test.",
        submitted_at: iso(300 + i),
      });
    }
  });
  const sections = ["needs_review", "ready", "waiting"] as const;
  const inbox: MockInboxEntry[] = pulls.map((p, i) => ({
    id: i + 1,
    section: sections[i % 3] as (typeof sections)[number],
    pinned: i === 0,
    pull_request: p,
  }));
  const statuses = ["success", "failure", "running"] as const;
  const runs: MockRun[] = pulls.map((p, i) => ({
    id: 10 + i,
    title: `ci for #${p.number}`,
    status: statuses[i % 3] as (typeof statuses)[number],
    prettyref: p.head.ref,
    pull: p.number,
    jobs: [{ id: 100 + i, name: "test", status: statuses[i % 3] as string }],
    logs: { [100 + i]: `run ${10 + i}: ${statuses[i % 3]}\n` },
  }));
  return {
    repo,
    user: { id: 1, login: "fixture", full_name: "Fixture Author" },
    pulls,
    reviews,
    inbox,
    runs,
    merged: [],
  };
}

const notFound = { message: "not found" };

/** A mock Forgejo over `data` (mutated by inbox PUTs and merges). */
export function createReviewForgejo(data: ReviewData): Hono {
  const app = new Hono().basePath("/api/v1");
  app.get("/user", (c) => c.json(data.user));
  app.get("/user/asc/inbox", (c) => {
    const section = c.req.query("section");
    return c.json(section ? data.inbox.filter((e) => e.section === section) : data.inbox);
  });
  app.put("/user/asc/inbox/:id", async (c) => {
    const entry = data.inbox.find((e) => String(e.id) === c.req.param("id"));
    if (!entry) return c.json(notFound, 404);
    const body = (await c.req.json().catch(() => ({}))) as {
      pinned?: unknown;
      snoozed_until?: unknown;
    };
    if (typeof body.pinned === "boolean") entry.pinned = body.pinned;
    if (typeof body.snoozed_until === "string") entry.snoozed_until = body.snoozed_until;
    return c.json(entry);
  });

  const scoped = new Hono();
  scoped.use("*", async (c, next) => {
    if (`${c.req.param("o")}/${c.req.param("r")}` !== data.repo) return c.json(notFound, 404);
    await next();
  });
  scoped.get("/pulls", (c) => c.json(data.pulls));
  scoped.get("/pulls/:n", (c) => {
    const p = data.pulls.find((x) => String(x.number) === c.req.param("n"));
    return p ? c.json(p) : c.json(notFound, 404);
  });
  scoped.get("/pulls/:n/reviews", (c) => {
    const list = data.reviews[Number(c.req.param("n"))];
    return list ? c.json(list) : c.json(notFound, 404);
  });
  scoped.post("/pulls/:n/merge", (c) => {
    const n = Number(c.req.param("n"));
    if (!data.pulls.some((p) => p.number === n)) return c.json(notFound, 404);
    data.merged.push(n);
    return c.body(null, 200);
  });
  scoped.get("/actions/runs", (c) =>
    c.json({
      workflow_runs: data.runs.map(({ jobs: _j, logs: _l, ...r }) => r),
      total_count: data.runs.length,
    }),
  );
  scoped.get("/actions/runs/:id", (c) => {
    const r = data.runs.find((x) => String(x.id) === c.req.param("id"));
    if (!r) return c.json(notFound, 404);
    const { jobs: _j, logs: _l, ...run } = r;
    return c.json(run);
  });
  scoped.get("/actions/runs/:id/jobs", (c) => {
    const r = data.runs.find((x) => String(x.id) === c.req.param("id"));
    return r ? c.json({ jobs: r.jobs, total_count: r.jobs.length }) : c.json(notFound, 404);
  });
  scoped.get("/actions/jobs/:id/logs", (c) => {
    const id = Number(c.req.param("id"));
    for (const r of data.runs) {
      const log = r.logs[id];
      if (log !== undefined) return c.text(log);
    }
    return c.json(notFound, 404);
  });
  app.route("/repos/:o/:r", scoped);
  app.all("*", (c) => c.json(notFound, 404));
  return app;
}
