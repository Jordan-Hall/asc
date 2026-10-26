import { execFileSync } from "node:child_process";
import { mkdtempSync, readFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { expect, type Page, test } from "@playwright/test";

const entry = join(import.meta.dirname, "../src/views/history/e2e-entry.ts");
let bundle = "";

test.beforeAll(() => {
  const out = join(mkdtempSync(join(tmpdir(), "asc-history-")), "history.js");
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

const st = (path: string, added: number, removed: number) => ({
  path,
  added,
  removed,
  binary: false,
});
const wt = (wsId: string, o: Record<string, unknown>) => ({
  wsId,
  path: `/repo-${wsId}`,
  mode: "",
  headRef: "x",
  dirty: 0,
  staged: 0,
  ahead: 0,
  behind: 0,
  locked: "",
  inProgress: "",
  current: false,
  main: false,
  ...o,
});
const data = {
  file: "src/new.ts",
  entries: [
    { id: "r3", subject: "big", author: "bob", time: 3e6, stats: [st("src/new.ts", 90, 10)] },
    { id: "r2", subject: "small", author: "amy", time: 2e6, stats: [st("src/new.ts", 1, 0)] },
    { id: "r1", subject: "created", author: "bob", time: 1e6, stats: [st("src/old.ts", 5, 0)] },
  ],
  worktrees: [
    wt("m", { path: "/repo", current: true, main: true }),
    wt("a", { dirty: 3, ahead: 1 }),
    wt("b", { behind: 2 }),
  ],
};

async function start(page: Page): Promise<void> {
  await page.goto("/");
  await expect(page.locator("#root")).toHaveText(/asc-ui ready/);
  await page.addScriptTag({ content: bundle, type: "module" });
  await page.waitForFunction(() => typeof window.ascHistoryStart === "function");
  await page.evaluate((d) => window.ascHistoryStart(d as never), data);
}

test("swim lanes per author and node size by churn", async ({ page }) => {
  await start(page);
  const dot = (id: string) => page.getByTestId(`rev-${id}`).locator(".asc-node");
  await expect(dot("r3")).toHaveAttribute("data-lane", "0");
  await expect(dot("r2")).toHaveAttribute("data-lane", "1");
  const size = async (id: string) => Number(await dot(id).getAttribute("data-size"));
  expect(await size("r3")).toBeGreaterThan(await size("r2"));
  await expect(page.getByTestId("rev-r2").locator('[data-badge="rename"]')).toHaveCount(1);
});

test("double click opens the diff at the followed path", async ({ page }) => {
  await start(page);
  await page.getByTestId("rev-r1").dblclick();
  const diffs = await page.evaluate(() => window.ascCalls.diffs);
  expect(diffs).toEqual([{ path: "src/old.ts", rev: "r1" }]);
});

test("worktrees: disabled remove states its reason, enabled remove calls the host", async ({
  page,
}) => {
  await start(page);
  await expect(page.getByTestId("wt-m").locator('[data-action="remove"]')).toHaveAttribute(
    "title",
    "The main worktree cannot be removed",
  );
  await expect(page.getByTestId("wt-a").locator('[data-action="remove"]')).toHaveAttribute(
    "title",
    "Uncommitted changes",
  );
  await page.getByTestId("wt-b").locator('[data-action="remove"]').click();
  await page.getByTestId("wt-a").locator('[data-action="open"]').click();
  const c = await page.evaluate(() => window.ascCalls);
  expect(c.removed).toEqual(["/repo-b"]);
  expect(c.opened).toEqual(["/repo-a"]);
});
