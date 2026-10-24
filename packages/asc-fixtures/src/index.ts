// @asc/asc-fixtures: deterministic fixture repositories built only with the asc CLI (SPEC section
// 24.7.2). `buildFixture("stacks", { dir })` builds a scenario under `dir`; building it twice
// gives the same commits, refs and stack shape (see `snapshot`).
import { join } from "node:path";
import { createReviewForgejo, type ReviewData, reviewDataFor, type StackChange } from "./forgejo";
import { type ReviewServer, startReviewServer } from "./server";
import {
  buildLocal,
  buildStacksInto,
  type BuildOptions,
  type Fixture,
  type LocalScenario,
  type ScenarioName,
  SCENARIOS,
} from "./scenarios";
import type { Hono } from "hono";
import type { RunResult } from "./asc";

export { Asc, ascAvailable, ascBinary, EPOCH, IDENTITY, type RunResult } from "./asc";
export { buildMbox, type FileEdit, type PatchCommit } from "./mbox";
export {
  createReviewForgejo,
  type MockInboxEntry,
  type MockPull,
  type MockReview,
  type MockRun,
  type ReviewData,
  reviewDataFor,
  type StackChange,
} from "./forgejo";
export { type ReviewServer, startReviewServer } from "./server";
export { type Snapshot, snapshot, supportsDeterministicChangeIds } from "./snapshot";
export {
  type BuildOptions,
  DEFAULT_SIZES,
  type Fixture,
  linearHistory,
  type ScenarioName,
  SCENARIOS,
  type Sizes,
} from "./scenarios";

/** The `review` scenario: `stacks`, a server, and the forge's view of the submitted changes. */
export interface ReviewFixture extends Fixture {
  readonly server: ReviewServer;
  readonly data: ReviewData;
  /** A mock Forgejo over `data`; hand it to `Bun.serve` or call `.fetch`. */
  readonly forgejo: Hono;
  /** The environment an asc client needs for the server (token, loopback http opt-in). */
  readonly clientEnv: Readonly<Record<string, string>>;
  /** What `asc submit` answered (the server holds no pull requests without a Forgejo). */
  readonly submit: RunResult;
}

export async function buildFixture(
  name: Exclude<ScenarioName, "review">,
  opts: BuildOptions,
): Promise<Fixture>;
export async function buildFixture(name: "review", opts: BuildOptions): Promise<ReviewFixture>;
export async function buildFixture(
  name: ScenarioName,
  opts: BuildOptions,
): Promise<Fixture | ReviewFixture>;
export async function buildFixture(
  name: ScenarioName,
  opts: BuildOptions,
): Promise<Fixture | ReviewFixture> {
  if (!(SCENARIOS as readonly string[]).includes(name)) throw new Error(`unknown scenario ${name}`);
  if (name !== "review") return buildLocal(name as LocalScenario, opts);
  return await buildReview(opts);
}

interface SlChange {
  change: string;
  short: string;
  subject: string;
  state: string;
}

export function stackChanges(fx: Fixture): StackChange[] {
  const sl = fx.asc.json<{ stacks: { changes: SlChange[] }[] }>(["sl", "--json"], { cwd: fx.root });
  const seen = new Set<string>();
  const out: StackChange[] = [];
  for (const s of sl.stacks) {
    for (const c of s.changes) {
      if (seen.has(c.change)) continue;
      seen.add(c.change);
      out.push({ change: c.short, subject: c.subject });
    }
  }
  return out;
}

async function buildReview(opts: BuildOptions): Promise<ReviewFixture> {
  const base = buildStacksInto(opts, "review");
  const { asc, root, marks } = base;
  // Bring stack A up to date so it can be submitted; stack B keeps its divergent change, so only
  // the part below it (b1 to b3) goes up.
  asc.ok(["switch", "-q", "stack-a"], { cwd: root });
  asc.run(
    [
      "restack",
      "--upstack",
      asc.ok(["change-id", "--short", marks.a2 as string], { cwd: root }).trim(),
    ],
    { cwd: root },
  );
  const data = reviewDataFor(stackChanges(base));
  const server = await startReviewServer(asc, join(opts.dir, "review", "server"), data.repo);
  const fx: ReviewFixture = {
    ...base,
    server,
    data,
    forgejo: createReviewForgejo(data),
    clientEnv: { ASC_TOKEN: server.token, ASC_ALLOW_LOOPBACK_HTTP: "1" },
    submit: { code: -1, stdout: "", stderr: "" },
    disposers: base.disposers,
    dispose: base.dispose,
  };
  fx.disposers.push(() => server.stop());
  try {
    // The token and the loopback opt-in travel in the environment, never in argv.
    const env = fx.clientEnv;
    asc.ok(["remote", "add", "origin", `${server.url}/${server.repo}`], { cwd: root, env });
    asc.run(["push", "origin", "master"], { cwd: root, env });
    (fx as { submit: RunResult }).submit = asc.run(["submit", "-s"], { cwd: root, env });
  } catch (e) {
    await fx.dispose();
    throw e;
  }
  return fx;
}
