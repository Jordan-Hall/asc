import { execFileSync } from "node:child_process";
import { mkdtempSync, readFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { expect, type Page, test } from "@playwright/test";

const entry = join(import.meta.dirname, "../src/views/smartlog/e2e-entry.ts");
let bundle = "";

test.beforeAll(() => {
  const out = join(mkdtempSync(join(tmpdir(), "asc-smartlog-")), "smartlog.js");
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
          change("a", undefined, { needsRestack: true }),
          change("b", "a", {
            current: true,
            review: {
              host: "f",
              number: 7,
              url: "",
              state: "open",
              draft: false,
              approvals: 1,
              checks: "passing",
            },
          }),
          change("c", "b", { frozen: true }),
        ],
      },
      { trunk: "master", base: "aa", behind: 0, changes: [change("x", undefined)] },
    ],
  },
  extras: { x: { divergent: true, worktree: "wt1" } },
  uncommitted: [
    { path: "src/a.ts", status: "M" },
    { path: "src/b.ts", status: "A" },
  ],
  hidden: ["a"],
  cloudDrafts: [{ id: "z", subject: "from laptop", machine: "laptop" }],
};

async function start(page: Page, conflicts = 0): Promise<void> {
  await page.goto("/");
  await expect(page.locator("#root")).toHaveText(/asc-ui ready/);
  await page.addScriptTag({ content: bundle, type: "module" });
  await page.waitForFunction(() => typeof window.ascStart === "function");
  await page.evaluate(([d, c]) => window.ascStart(d as never, c as number), [
    data,
    conflicts,
  ] as const);
}

test("rows show PR, divergent, frozen and worktree state; hidden and cloud are toggled", async ({
  page,
}) => {
  await start(page);
  await expect(page.getByTestId("row-b").locator('[data-badge="pr"]')).toHaveText("#7 open");
  await expect(page.getByTestId("row-c").locator('[data-badge="frozen"]')).toBeVisible();
  await expect(page.getByTestId("row-x").locator('[data-badge="divergent"]')).toBeVisible();
  await expect(page.getByTestId("row-x").locator('[data-badge="worktree"]')).toHaveText("wt1");
  await expect(page.getByTestId("row-a")).toHaveCount(0);
  await page.getByLabel("Show hidden").check();
  await expect(page.getByTestId("row-a").locator('[data-badge="restack"]')).toBeVisible();
  await expect(page.getByTestId("cloud-z")).toHaveCount(0);
  await page.getByLabel("Show cloud drafts").check();
  await expect(page.getByTestId("cloud-z")).toContainText("laptop");
});

test("disabled buttons state their reason", async ({ page }) => {
  await start(page);
  const checkout = page.getByTestId("row-x").locator('[data-action="checkout"]');
  await expect(checkout).toHaveAttribute(
    "title",
    "Uncommitted changes: create or stash them first",
  );
  await expect(page.getByTestId("row-c").locator('[data-action="abandon"]')).toHaveAttribute(
    "title",
    "Frozen",
  );
  const create = page.getByTestId("row-uncommitted").locator('[data-action="create"]');
  await expect(create).toHaveAttribute("title", "Select at least one file");
});

test("uncommitted files are checked, then create runs with exactly those paths", async ({
  page,
}) => {
  await start(page);
  await page.locator('input[data-path="src/b.ts"]').check();
  await page.getByTestId("row-uncommitted").locator('[data-action="create"]').click();
  const runs = await page.evaluate(() => window.ascSmartlog.calls.runs);
  expect(runs).toEqual([{ verb: "create", files: ["src/b.ts"] }]);
});

test("drag shows the predicted-conflict badge, then enqueues the move", async ({ page }) => {
  await start(page, 2);
  await page.getByTestId("row-x").dragTo(page.getByTestId("row-b"));
  const calls = await page.evaluate(() => window.ascSmartlog.calls);
  expect(calls.predictions).toEqual([{ change: "x", onto: { kind: "change", id: "b" } }]);
  expect(calls.moves).toEqual([{ change: "x", onto: { kind: "change", id: "b" } }]);
});

test("a frozen change cannot be dragged and a change cannot drop on its descendant", async ({
  page,
}) => {
  await start(page);
  await expect(page.getByTestId("row-c")).toHaveAttribute("draggable", "false");
  await page.getByLabel("Show hidden").check();
  await page.getByTestId("row-a").dragTo(page.getByTestId("row-b"));
  const calls = await page.evaluate(() => window.ascSmartlog.calls);
  expect(calls.moves).toEqual([]);
});

test("double click checks out an enabled change", async ({ page }) => {
  await page.goto("/");
  await page.addScriptTag({ content: bundle, type: "module" });
  await page.waitForFunction(() => typeof window.ascStart === "function");
  await page.evaluate(
    (d) => window.ascStart({ ...(d as object), uncommitted: [], extras: {} } as never, 0),
    data,
  );
  await page.getByTestId("row-x").dblclick();
  const runs = await page.evaluate(() => window.ascSmartlog.calls.runs);
  expect(runs).toEqual([{ verb: "switch", change: "x" }]);
});
