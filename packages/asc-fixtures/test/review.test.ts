// The `review` scenario: the stacks scenario against a real `asc serve` on loopback, with a mock
// Forgejo carrying the pull requests, reviews, inbox entries and Actions runs.
import { afterAll, describe, expect, setDefaultTimeout, test } from "bun:test";
import { mkdtempSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { createForgejoClient } from "@asc/asc-client/forgejo";
import {
  buildFixture,
  ascAvailable,
  createReviewForgejo,
  type ReviewFixture,
  reviewDataFor,
} from "../src/index";

setDefaultTimeout(3_600_000);

describe("mock forgejo", () => {
  const changes = ["a2", "a3", "b1", "b2", "b3", "b5"].map((c) => ({
    change: c,
    subject: `feat: ${c}`,
  }));
  const client = () => {
    const app = createReviewForgejo(reviewDataFor(changes));
    return createForgejoClient({
      baseUrl: "https://forge.test",
      fetch: (r) => Promise.resolve(app.fetch(r)),
      repos: ["asc/demo"],
    });
  };

  test("the data is a pure function of the changes", () => {
    expect(reviewDataFor(changes)).toEqual(reviewDataFor(changes));
    const d = reviewDataFor(changes);
    expect(d.pulls.map((p) => p.head.ref)).toEqual(changes.map((c) => `asc-${c.change}`));
    expect(
      Object.values(d.reviews)
        .flat()
        .map((r) => r.state),
    ).toContain("APPROVED");
    expect(
      Object.values(d.reviews)
        .flat()
        .map((r) => r.state),
    ).toContain("REQUEST_CHANGES");
    expect(new Set(d.runs.map((r) => r.status))).toEqual(
      new Set(["success", "failure", "running"]),
    );
  });

  test("the client reads the inbox and pins an entry", async () => {
    const c = client();
    const all = await c.api.GET("/user/asc/inbox");
    expect(all.data?.length).toBe(changes.length);
    const pinned = await c.api.PUT("/user/asc/inbox/{id}", {
      params: { path: { id: 2 } },
      body: { pinned: true },
    });
    expect(pinned.data?.pinned).toBe(true);
  });

  test("pulls, reviews and runs answer; other repositories do not", async () => {
    const app = createReviewForgejo(reviewDataFor(changes));
    const get = async (p: string) => app.fetch(new Request(`https://forge.test/api/v1${p}`));
    expect((await get("/repos/asc/demo/pulls")).status).toBe(200);
    expect(
      ((await (await get("/repos/asc/demo/pulls/1/reviews")).json()) as unknown[]).length,
    ).toBe(1);
    expect(
      ((await (await get("/repos/asc/demo/actions/runs")).json()) as { total_count: number })
        .total_count,
    ).toBe(changes.length);
    expect((await get("/repos/asc/demo/actions/jobs/100/logs")).status).toBe(200);
    expect((await get("/repos/other/repo/pulls")).status).toBe(404);
    expect((await get("/nope")).status).toBe(404);
  });
});

const scratch = mkdtempSync(join(tmpdir(), "asc-fixtures-review-"));
afterAll(() => rmSync(scratch, { recursive: true, force: true }));

describe("review scenario", () => {
  test.skipIf(!ascAvailable())(
    "a server, a token that never reaches argv, and the stacks' pull requests",
    async () => {
      const fx: ReviewFixture = await buildFixture("review", { dir: scratch });
      try {
        const ready = await fetch(`${fx.server.adminUrl}/readyz`);
        expect(ready.ok).toBe(true);
        expect(fx.server.url.startsWith("http://127.0.0.1:")).toBe(true); // loopback only
        expect(fx.data.pulls.length).toBeGreaterThanOrEqual(8);
        expect(fx.asc.ok(["remote", "get-url", "origin"], { cwd: fx.root }).trim()).toBe(
          `${fx.server.url}/${fx.server.repo}`,
        );
        // The submit ran against the live server; whatever it answered, it did not leak the token.
        expect(fx.submit.code).not.toBe(-1);
        expect(fx.submit.stdout + fx.submit.stderr).not.toContain(fx.server.token);
        const inbox = await fx.forgejo.fetch(
          new Request("https://forge.test/api/v1/user/asc/inbox"),
        );
        expect(inbox.status).toBe(200);
      } finally {
        await fx.dispose();
      }
      await expect(fetch(`${fx.server.adminUrl}/readyz`)).rejects.toThrow();
    },
  );
});
