import { expect, type Page, test } from "@playwright/test";
import { hexId } from "../src/views/graph/fixture";
import { buildGraphBundle, startGraph } from "../src/views/graph/e2e-helpers";

let bundle = "";
test.beforeAll(() => {
  bundle = buildGraphBundle();
});

const row = (id: number) => `grow-${hexId(id).slice(0, 8)}`;
const selectedIndex = (page: Page) => page.evaluate(() => window.ascGraph.view.selectedIndex);

test("the first page renders virtual rows and a lane canvas, not 200 DOM rows", async ({
  page,
}) => {
  await startGraph(page, bundle, { total: 1000 });
  await expect(page.getByTestId(row(0))).toContainText("commit 0");
  await expect(page.getByTestId(row(0))).toContainText("main");
  expect(await page.locator(".asc-grow").count()).toBeLessThan(80);
  await expect(page.locator(".asc-graph-lanes canvas")).toHaveCount(1);
  await expect(page.getByTestId("graph-status")).toHaveText("1000 commits");
});

test("scrolling loads further pages and keeps at most 5,000 rows in memory", async ({ page }) => {
  await startGraph(page, bundle, { total: 20_000 });
  await expect(page.getByTestId(row(0))).toBeVisible();
  await page.evaluate(() => {
    const s = document.querySelector<HTMLElement>('[data-testid="graph-scroll"]');
    if (s) s.scrollTop = 15_000 * 22;
  });
  await expect(page.locator('[data-index="15000"]')).toContainText("commit 15000");
  await page.evaluate(() => window.ascGraph.view.idle());
  expect(await page.evaluate(() => window.ascGraph.view.heldRows)).toBeLessThanOrEqual(5000);
});

test("keyboard: arrows move, left goes to the parent, right to the child, r and F jump", async ({
  page,
}) => {
  await startGraph(page, bundle, { total: 1000 });
  await expect(page.getByTestId(row(0))).toBeVisible();
  await page.getByTestId("graph-scroll").focus();
  await page.keyboard.press("ArrowDown");
  await page.keyboard.press("ArrowDown");
  await expect.poll(() => selectedIndex(page)).toBe(1);
  await page.keyboard.press("ArrowDown"); // row 2
  await page.keyboard.press("ArrowDown"); // row 3, parent is the fork row 4
  await page.keyboard.press("ArrowLeft");
  await expect.poll(() => selectedIndex(page)).toBe(4);
  await page.keyboard.press("ArrowRight");
  await expect.poll(() => selectedIndex(page)).toBe(3);
  await page.keyboard.press("r"); // next ref after row 3 is tag-50
  await expect.poll(() => selectedIndex(page)).toBe(50);
  await page.keyboard.press("F"); // previous fork before row 50 is the merge at row 25
  await expect.poll(() => selectedIndex(page)).toBe(25);
  await page.keyboard.press("Enter");
  expect(await page.evaluate(() => window.ascGraph.calls.opened.length)).toBe(1);
});

test("slash focuses the search and Enter jumps to the next match among loaded rows", async ({
  page,
}) => {
  await startGraph(page, bundle, { total: 1000 });
  await expect(page.getByTestId(row(0))).toBeVisible();
  await page.getByTestId("graph-scroll").focus();
  await page.keyboard.press("/");
  await expect(page.getByLabel("Search loaded rows")).toBeFocused();
  await page.keyboard.type("commit 42");
  await page.keyboard.press("Enter");
  await expect.poll(() => selectedIndex(page)).toBe(42);
});

test("changing the preset and ref filters refetches from the first page with the query", async ({
  page,
}) => {
  await startGraph(page, bundle, { total: 500 });
  await expect(page.getByTestId(row(0))).toBeVisible();
  await page.getByLabel("Include refs").fill("main, feat");
  await page.getByLabel("Include refs").dispatchEvent("change");
  await page.getByLabel("Preset").selectOption("stack");
  await page.evaluate(() => window.ascGraph.view.idle());
  const q = await page.evaluate(() => window.ascGraph.calls.queries.at(-1));
  expect(q).toMatchObject({
    preset: "stack",
    include: ["main", "feat"],
    exclude: [],
    pageToken: "",
    limit: 200,
  });
});

test("the context menu runs cherry-pick and revert with the full id; a merge disables them with a reason", async ({
  page,
}) => {
  await startGraph(page, bundle, { total: 1000 });
  await expect(page.getByTestId(row(10))).toBeVisible();
  await page.getByTestId(row(10)).click({ button: "right" });
  await page.getByTestId("graph-menu").locator('[data-action="cherry-pick"]').click();
  await page.getByTestId(row(10)).click({ button: "right" });
  await page.getByTestId("graph-menu").locator('[data-action="revert"]').click();
  const runs = await page.evaluate(() => window.ascGraph.calls.runs);
  const id = hexId(10);
  expect(runs).toEqual([
    { verb: "cherry-pick", rev: id },
    { verb: "revert", rev: id },
  ]);
  await page.getByTestId(row(0)).click({ button: "right" });
  const cp = page.getByTestId("graph-menu").locator('[data-action="cherry-pick"]');
  await expect(cp).toBeDisabled();
  await expect(cp).toHaveAttribute("title", /Merge commit/);
});

test("a click on the minimap jumps to that part of the history", async ({ page }) => {
  await startGraph(page, bundle, { total: 10_000 });
  await expect(page.getByTestId(row(0))).toBeVisible();
  const box = await page.getByTestId("minimap").boundingBox();
  if (!box) throw new Error("minimap not laid out");
  await page.mouse.click(box.x + box.width / 2, box.y + box.height / 2);
  await expect
    .poll(() =>
      page.evaluate(
        () => document.querySelector<HTMLElement>('[data-testid="graph-scroll"]')?.scrollTop ?? 0,
      ),
    )
    .toBeGreaterThan(4000 * 22);
});

test("a Watch refresh fetches the first page only", async ({ page }) => {
  await startGraph(page, bundle, { total: 1000 });
  await expect(page.getByTestId(row(0))).toBeVisible();
  await page.evaluate(() => window.ascGraph.view.idle());
  const before = await page.evaluate(() => window.ascGraph.calls.queries.length);
  await page.evaluate(() => window.ascBumpHead());
  const added = await page.evaluate((n) => window.ascGraph.calls.queries.slice(n), before);
  expect(added.length).toBeGreaterThanOrEqual(1);
  expect(added[0]?.pageToken).toBe("");
});
