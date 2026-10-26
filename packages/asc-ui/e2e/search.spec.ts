import { execFileSync } from "node:child_process";
import { mkdtempSync, readFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { expect, type Page, test } from "@playwright/test";

const entry = join(import.meta.dirname, "../src/views/search/e2e-entry.ts");
let bundle = "";

test.beforeAll(() => {
  const out = join(mkdtempSync(join(tmpdir(), "asc-search-")), "search.js");
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

const items = [
  { path: "src/a.ts", line: 3, text: "const needle = 1;", cols: [6], length: 6, redacted: false },
  {
    path: "src/<b>.ts",
    line: 9,
    text: "<img src=x onerror=alert(1)> needle",
    cols: [29],
    length: 6,
    redacted: false,
  },
  { path: "secret.env", line: 1, text: "TOKEN=abc", cols: [], length: 0, redacted: true },
];

async function start(
  page: Page,
  searchV1: boolean,
  coverage = [{ index: "trigram", coverage: 0.5, lagSeconds: 12 }],
) {
  await page.goto("/");
  await expect(page.locator("#root")).toHaveText(/asc-ui ready/);
  await page.addScriptTag({ content: bundle, type: "module" });
  await page.waitForFunction(() => typeof window.ascStartSearch === "function");
  await page.evaluate((o) => window.ascStartSearch(o as never), { searchV1, items, coverage });
}

test("exact search with search.v1 shows hits, highlight and index coverage", async ({ page }) => {
  await start(page, true);
  await page.getByTestId("search-input").fill("needle");
  await page.getByTestId("search-go").click();
  await expect(page.getByTestId("results").locator("li")).toHaveCount(3);
  await expect(page.getByTestId("results").locator("mark").first()).toHaveText("needle");
  await expect(page.getByTestId("coverage")).toHaveText("Index: trigram 50%, 12s behind");
  await expect(page.getByTestId("coverage")).toHaveClass(/asc-partial/);
  await expect(page.getByTestId("status")).toHaveText("3 results");
});

test("result text is never parsed as markup and redacted hits hide content", async ({ page }) => {
  await start(page, true);
  await page.getByTestId("search-input").fill("needle");
  await page.getByTestId("search-go").click();
  await expect(page.getByTestId("results").locator("li")).toHaveCount(3);
  await expect(page.getByTestId("results").locator("img")).toHaveCount(0);
  await expect(page.getByTestId("results")).toContainText("<img src=x onerror=alert(1)>");
  await expect(page.getByTestId("results")).not.toContainText("TOKEN=abc");
  await expect(page.getByTestId("results")).toContainText("Content hidden by policy");
});

test("without search.v1 only exact is enabled and the others state why", async ({ page }) => {
  await start(page, false);
  await expect(page.locator('[data-mode="exact"]')).toHaveAttribute("aria-disabled", "false");
  for (const m of ["regex", "symbol", "semantic"]) {
    const b = page.locator(`[data-mode="${m}"]`);
    await expect(b).toHaveAttribute("aria-disabled", "true");
    await expect(b).toHaveAttribute("title", /search\.v1/);
  }
  await expect(page.getByTestId("coverage")).toContainText("No index");
  await expect(page.locator('[data-testid="scope"] option[value="stack"]')).toBeDisabled();
});

test("the scope picker passes its value and submit is blocked with a reason when empty", async ({
  page,
}) => {
  await start(page, true);
  await expect(page.getByTestId("search-go")).toHaveAttribute("title", "Enter a search term");
  await page.getByTestId("search-input").fill("needle");
  await page.getByTestId("scope").selectOption("change");
  await expect(page.getByTestId("search-go")).toHaveAttribute(
    "title",
    "Enter a revision, change or stack id",
  );
  await page.getByTestId("scope-value").fill("abc123");
  await page.locator('[data-mode="regex"]').click();
  await page.getByTestId("search-go").click();
  await expect(page.getByTestId("results").locator("li")).toHaveCount(3);
  const searches = await page.evaluate(() => window.ascSearch.calls.searches);
  expect(searches).toEqual([
    {
      mode: "regex",
      text: "needle",
      scope: { kind: "change", value: "abc123" },
      caseSensitive: false,
    },
  ]);
});

test("clicking a hit opens it", async ({ page }) => {
  await start(page, true);
  await page.getByTestId("search-input").fill("needle");
  await page.getByTestId("search-go").click();
  await page.getByTestId("results").locator("li").first().click();
  const opened = await page.evaluate(() => window.ascSearch.calls.opened);
  expect(opened).toEqual([{ path: "src/a.ts", line: 3 }]);
});
