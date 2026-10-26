import { execFileSync } from "node:child_process";
import { mkdtempSync, readFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { expect, type Page, test } from "@playwright/test";

const entry = join(import.meta.dirname, "../src/views/stack-editor/e2e-entry.ts");
let bundle = "";

test.beforeAll(() => {
  const out = join(mkdtempSync(join(tmpdir(), "asc-stack-editor-")), "stack-editor.js");
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

const change = (id: string, frozen = false) => ({
  id,
  subject: `subject ${id}`,
  branches: [],
  state: "draft",
  needsRestack: false,
  frozen,
  empty: false,
  current: false,
});

const stack = {
  trunk: "master",
  base: "aa",
  behind: 0,
  changes: [change("aaaa"), change("bbbb"), change("cccc", true), change("dddd")],
};

async function start(page: Page, fail?: string): Promise<void> {
  await page.goto("/");
  await expect(page.locator("#root")).toHaveText(/asc-ui ready/);
  await page.addScriptTag({ content: bundle, type: "module" });
  await page.waitForFunction(() => typeof window.ascStartEditor === "function");
  await page.evaluate(([s, f]) => window.ascStartEditor(s as never, f as string | undefined), [
    stack,
    fail,
  ] as const);
}

const row = (page: Page, id: string) => page.getByTestId(`se-row-${id}`);
const apply = (page: Page) => page.locator('[data-action="apply"]');

test("apply is disabled until an edit, and says why", async ({ page }) => {
  await start(page);
  await expect(apply(page)).toBeDisabled();
  await expect(apply(page)).toHaveAttribute("title", "No edits");
  await expect(row(page, "cccc").locator('[data-action="up"]')).toHaveAttribute("title", "Frozen");
});

test("reorder, fold, drop and reword commit as one todo", async ({ page }) => {
  await start(page);
  await row(page, "bbbb").getByTestId("se-action").selectOption("fold");
  await row(page, "dddd").getByTestId("se-action").selectOption("drop");
  await row(page, "aaaa").getByTestId("se-action").selectOption("reword");
  await row(page, "aaaa").getByTestId("se-message").fill("better words");
  await apply(page).click();
  await expect.poll(() => page.evaluate(() => window.ascEditor.todos.length)).toBe(1);
  const todos = await page.evaluate(() => window.ascEditor.todos);
  expect(todos[0]?.items).toEqual([
    { action: "reword", change: "aaaa", message: "better words" },
    { action: "fold", change: "bbbb" },
    { action: "pick", change: "cccc" },
    { action: "drop", change: "dddd" },
  ]);
});

test("dragging a row onto another reorders; frozen rows do not drag", async ({ page }) => {
  await start(page);
  await expect(row(page, "cccc")).toHaveAttribute("draggable", "false");
  await row(page, "dddd").dragTo(row(page, "aaaa"));
  await apply(page).click();
  const todos = await page.evaluate(() => window.ascEditor.todos);
  expect(todos[0]?.items.map((i: { change: string }) => i.change)).toEqual([
    "dddd",
    "aaaa",
    "bbbb",
    "cccc",
  ]);
});

test("a failed apply shows the error and keeps the edits", async ({ page }) => {
  await start(page, "the todo conflicts");
  await row(page, "bbbb").getByTestId("se-action").selectOption("drop");
  await apply(page).click();
  await expect(page.getByTestId("se-error")).toHaveText("the todo conflicts");
  await expect(row(page, "bbbb").getByTestId("se-action")).toHaveValue("drop");
});
