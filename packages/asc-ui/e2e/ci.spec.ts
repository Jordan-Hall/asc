import { execFileSync } from "node:child_process";
import { mkdtempSync, readFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { expect, type Page, test } from "@playwright/test";

const entry = join(import.meta.dirname, "../src/views/actions/e2e-entry.ts");
let bundle = "";

test.beforeAll(() => {
  const out = join(mkdtempSync(join(tmpdir(), "asc-ci-")), "ci.js");
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

const longLog = `${Array.from({ length: 300 }, (_, i) => `line ${i}`).join("\n")}\n\u001b[31mred\u001b[0m <b>x</b>\n`;
const data = {
  runs: [
    { id: 1, title: "build", status: "success", commit: "abcdef1234" },
    { id: 2, title: "test", status: "running" },
  ],
  jobs: { 1: [{ id: 10, name: "unit", status: "success" }] },
  logs: { 10: longLog },
  affected: [
    {
      id: "b",
      name: "pkg-b",
      root: "b",
      direct: false,
      reasons: ["depends on pkg-a"],
      targets: {},
    },
    {
      id: "a",
      name: "pkg-a",
      root: "a",
      direct: true,
      reasons: ["files changed"],
      targets: { build: ["bun", "run", "build"] },
    },
  ],
  projects: [
    { id: "a", name: "pkg-a", deps: [] },
    { id: "b", name: "pkg-b", deps: ["a"] },
  ],
};

async function start(page: Page): Promise<void> {
  await page.goto("/");
  await expect(page.locator("#root")).toHaveText(/asc-ui ready/);
  await page.addScriptTag({ content: bundle, type: "module" });
  await page.waitForFunction(() => typeof window.ascStartCi === "function");
  await page.evaluate((d) => window.ascStartCi(d as never), data);
}

test("runs list shows status; a running run cannot be re-run and says why", async ({ page }) => {
  await start(page);
  await expect(page.getByTestId("run-1").locator('[data-badge="status"]')).toHaveText("success");
  await expect(page.getByTestId("run-2").locator('[data-action="rerun"]')).toHaveAttribute(
    "title",
    "Run has not finished",
  );
  await page.getByTestId("run-1").locator('[data-action="rerun"]').click();
  expect(await page.evaluate(() => window.ascCi.calls.reruns)).toEqual([1]);
});

test("job log shows the tail, strips escapes and never parses markup", async ({ page }) => {
  await start(page);
  await page.getByTestId("run-1").click();
  await page.getByTestId("job-10").click();
  const log = page.getByTestId("log");
  await expect(log).toContainText("earlier output not shown");
  await expect(log).toContainText("red <b>x</b>");
  await expect(log).not.toContainText("line 0\n");
  await expect(log).not.toContainText("\u001b");
  await expect(log.locator("b")).toHaveCount(0);
});

test("affected list puts direct projects first and runs a target's argv", async ({ page }) => {
  await start(page);
  const rows = page.locator('[data-testid^="proj-"]');
  await expect(rows.first()).toHaveAttribute("data-testid", "proj-a");
  await page.getByTestId("proj-a").locator('[data-action="run-build"]').click();
  expect(await page.evaluate(() => window.ascCi.calls.targets)).toEqual([["bun", "run", "build"]]);
  await page.getByLabel("Filter projects").fill("pkg-b");
  await expect(rows).toHaveCount(1);
});

test("project graph draws edges and dims non-neighbours on focus", async ({ page }) => {
  await start(page);
  await expect(page.locator('[data-edge="b>a"]')).toHaveCount(1);
  await page.getByTestId("node-a").click();
  await expect(page.getByTestId("node-b")).not.toHaveAttribute("opacity", "0.3");
  await page.getByTestId("node-b").dblclick();
  expect(await page.evaluate(() => window.ascCi.calls.opened)).toContain("b");
});
