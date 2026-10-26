import { execFileSync } from "node:child_process";
import { mkdtempSync, readFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { expect, type Page, test } from "@playwright/test";

const entry = join(import.meta.dirname, "../src/views/review/e2e-entry.ts");
let bundle = "";

test.beforeAll(() => {
  const out = join(mkdtempSync(join(tmpdir(), "asc-review-")), "review.js");
  // shell: bun is a .cmd shim on Windows; the arguments are fixed paths, never user input.
  execFileSync(
    "bun",
    ["build", `"${entry}"`, "--target=browser", "--format=esm", "--outfile", `"${out}"`],
    {
      stdio: "pipe",
      shell: true,
    },
  );
  bundle = readFileSync(out, "utf8");
});

const review = (n: number, extra: Record<string, unknown> = {}) => ({
  host: "f",
  number: n,
  url: "",
  state: "open",
  draft: false,
  approvals: 1,
  checks: "passing",
  ...extra,
});
const change = (id: string, parent: string | undefined, extra: Record<string, unknown> = {}) => ({
  id,
  ...(parent === undefined ? {} : { parent }),
  subject: `subject ${id}`,
  branches: [],
  state: "draft",
  needsRestack: false,
  frozen: false,
  empty: false,
  current: false,
  ...extra,
});
const v = (n: number) => ({ v: n, at: n, actor: "bob" });

const data = {
  stacks: {
    version: 1,
    trunks: [{ name: "master", tip: "aa", remote: "origin" }],
    stacks: [
      {
        trunk: "master",
        base: "aa",
        behind: 0,
        changes: [
          change("a", undefined, { review: review(1) }),
          change("b", "a", { review: review(2) }),
        ],
      },
    ],
  },
  selected: "a",
  changes: {
    a: {
      versions: [v(1), v(2), v(3)],
      lastReviewedV: 1,
      mergeability: "clean",
      overlaps: [{ change: "z", pr: 9, paths: ["src/x.ts"] }],
    },
    b: { versions: [v(1)], mergeability: "conflicts", conflictPaths: ["src/y.ts"] },
  },
  comments: {
    a: [
      { id: "c1", path: "src/x.ts", line: 4, author: "bob", body: "why?", resolved: false },
      { id: "c2", path: "src/x.ts", line: 9, author: "bob", body: "done", resolved: true },
    ],
  },
};
const files = [
  { path: "src/x.ts", status: "M", additions: 3, deletions: 1 },
  { path: "src/new.ts", status: "A", additions: 10, deletions: 0 },
];

async function start(page: Page, confirm = true): Promise<void> {
  await page.goto("/");
  await expect(page.locator("#root")).toHaveText(/asc-ui ready/);
  await page.addScriptTag({ content: bundle, type: "module" });
  await page.waitForFunction(() => typeof window.ascStartReview === "function");
  await page.evaluate(([d, f, c]) => window.ascStartReview(d as never, f as never, c as boolean), [
    data,
    files,
    confirm,
  ] as const);
}

test("navigator shows PR, mergeability, overlap and comment badges", async ({ page }) => {
  await start(page);
  const a = page.getByTestId("nav-a");
  await expect(a.locator('[data-badge="pr"]')).toHaveText("#1 open");
  await expect(a.locator('[data-badge="mergeability"]')).toHaveText("merges cleanly");
  await expect(a.locator('[data-badge="overlap"]')).toHaveText("overlaps 1");
  await expect(a.locator('[data-badge="comments"]')).toHaveText("1 open");
  await expect(page.getByTestId("nav-b").locator('[data-badge="mergeability"]')).toHaveText(
    "conflicts",
  );
});

test("default interdiff is previous against latest and lists the files", async ({ page }) => {
  await start(page);
  await expect(page.getByTestId("file-src/x.ts")).toContainText("M src/x.ts +3 -1");
  const calls = await page.evaluate(() => window.ascReview.calls);
  expect(calls.interdiffs).toEqual([{ change: "a", pick: { from: 2, to: 3 } }]);
  await page.getByTestId("file-src/new.ts").click();
  const opened = await page.evaluate(() => window.ascReview.calls.opened);
  expect(opened).toEqual([{ change: "a", path: "src/new.ts", pick: { from: 2, to: 3 } }]);
});

test("since my last review picks the last reviewed version against the latest", async ({
  page,
}) => {
  await start(page);
  await page.locator('[data-action="since-last-review"]').click();
  await expect(page.getByTestId("version-from")).toHaveValue("1");
  const calls = await page.evaluate(() => window.ascReview.calls.interdiffs);
  expect(calls.at(-1)).toEqual({ change: "a", pick: { from: 1, to: 3 } });
});

test("changing the version picker reloads the interdiff", async ({ page }) => {
  await start(page);
  await page.getByTestId("version-to").selectOption("2");
  const calls = await page.evaluate(() => window.ascReview.calls.interdiffs);
  expect(calls.at(-1)).toEqual({ change: "a", pick: { from: 2, to: 2 } });
});

test("comment list shows open and resolved threads", async ({ page }) => {
  await start(page);
  await expect(page.getByTestId("thread-c1")).toHaveText("src/x.ts:4 bob: why?");
  await expect(page.getByTestId("thread-c2")).toContainText("(resolved)");
});

test("merge states its reason, and only runs after the host confirms", async ({ page }) => {
  await start(page, false);
  await page.getByTestId("nav-b").click();
  await expect(page.locator('[data-action="merge"]')).toHaveAttribute(
    "title",
    "Merge #1 first (stack order)",
  );
  await page.getByTestId("nav-a").click();
  await page.locator('[data-action="merge"]').click();
  let calls = await page.evaluate(() => window.ascReview.calls);
  expect(calls.confirms).toEqual(['Merge #1 "subject a" into the target branch?']);
  expect(calls.merges).toEqual([]);
});

test("approve and merge run after confirmation", async ({ page }) => {
  await start(page, true);
  await page.locator('[data-action="approve"]').click();
  await page.locator('[data-action="merge"]').click();
  await expect.poll(() => page.evaluate(() => window.ascReview.calls.merges)).toEqual(["a"]);
  const calls = await page.evaluate(() => window.ascReview.calls);
  expect(calls.approves).toEqual(["a"]);
  expect(calls.confirms).toHaveLength(2);
});
