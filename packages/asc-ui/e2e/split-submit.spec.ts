import { execFileSync } from "node:child_process";
import { mkdtempSync, readFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { expect, type Page, test } from "@playwright/test";

const views = join(import.meta.dirname, "../src/views");
const bundles: Record<string, string> = {};

test.beforeAll(() => {
  const dir = mkdtempSync(join(tmpdir(), "asc-split-submit-"));
  for (const name of ["split", "submit"]) {
    const out = join(dir, `${name}.js`);
    // shell: bun is a .cmd shim on Windows; the arguments are fixed paths, never user input.
    execFileSync(
      "bun",
      ["build", `"${join(views, name, "e2e-entry.ts")}"`, "--target=browser", "--format=esm", "--outfile", `"${out}"`],
      { stdio: "pipe", shell: true },
    );
    bundles[name] = readFileSync(out, "utf8");
  }
});

const diff = [
  "diff --git a/f.txt b/f.txt",
  "index 1111111..2222222 100644",
  "--- a/f.txt",
  "+++ b/f.txt",
  "@@ -1,4 +1,4 @@",
  " a",
  "-b",
  "+B",
  " c",
  " d",
  "@@ -9,4 +9,5 @@",
  " i",
  " j",
  "-k",
  "+K",
  "+k2",
  " l",
  "diff --git a/img.png b/img.png",
  "index 1..2 100644",
  "Binary files a/img.png and b/img.png differ",
  "",
].join("\n");

async function startSplit(page: Page, d = diff, truncated = false, fail?: string): Promise<void> {
  await page.goto("/");
  await expect(page.locator("#root")).toHaveText(/asc-ui ready/);
  await page.addScriptTag({ content: bundles.split ?? "", type: "module" });
  await page.waitForFunction(() => typeof window.ascStartSplit === "function");
  await page.evaluate(([a, b, c]) => window.ascStartSplit(a as string, b as boolean, c as string | undefined), [d, truncated, fail] as const);
}

const apply = (page: Page) => page.locator('[data-action="apply"]');

test("split: picking hunks, lines and a whole file sends one SplitSpec", async ({ page }) => {
  await startSplit(page);
  await expect(apply(page)).toBeDisabled();
  await expect(apply(page)).toHaveAttribute("title", "Piece 1 has no changes");
  await page.getByTestId("sp-hunk-0-0").click();
  await page.getByTestId("sp-line-0-1-2").check();
  await page.getByTestId("sp-line-0-1-3").check();
  await page.getByTestId("sp-whole-1").check();
  await expect(apply(page)).toHaveAttribute("title", "Piece 1 needs a message");
  await page.getByTestId("sp-message-0").fill("first piece");
  await expect(apply(page)).toBeEnabled();
  await apply(page).click();
  await expect.poll(() => page.evaluate(() => window.ascSplit.specs.length)).toBe(1);
  const spec = await page.evaluate(() => window.ascSplit.specs[0]);
  expect(spec?.pieces).toHaveLength(2);
  expect(spec?.pieces[0]?.message).toBe("first piece");
  expect(spec?.pieces[0]?.wholeFiles).toEqual(["img.png"]);
  const patch = await page.evaluate((p) => atob(p ?? ""), spec?.pieces[0]?.patch);
  expect(patch).toContain("@@ -1,4 +1,4 @@");
  expect(patch).toContain("-k\n+K\n");
  expect(patch).not.toContain("k2");
  expect(spec?.pieces[1]).toEqual({ message: "Original subject" });
});

test("split: add and remove pieces, and a truncated diff is refused", async ({ page }) => {
  await startSplit(page);
  await page.locator('[data-action="add-piece"]').click();
  await expect(page.getByTestId("sp-piece-2")).toBeVisible();
  await page.locator('[data-action="remove-piece"]').click();
  await expect(page.getByTestId("sp-piece-2")).toHaveCount(0);
  await expect(page.locator('[data-action="remove-piece"]')).toBeDisabled();

  await startSplit(page, diff, true);
  await expect(apply(page)).toBeDisabled();
  await expect(apply(page)).toHaveAttribute("title", "The diff is truncated");
});

test("split: a failing run shows the error and keeps the dialog", async ({ page }) => {
  await startSplit(page, diff, false, "patch does not apply");
  await page.getByTestId("sp-hunk-0-0").click();
  await page.getByTestId("sp-message-0").fill("m");
  await apply(page).click();
  await expect(page.getByTestId("sp-error")).toHaveText("patch does not apply");
});

const changes = [
  { id: "aaaa", message: "Add thing\n\nLong words.", pull: { number: 7, title: "Add thing", body: "Long words.", draft: true } },
  { id: "bbbb", message: "Second change" },
];

async function startSubmit(page: Page, fail?: string): Promise<void> {
  await page.goto("/");
  await expect(page.locator("#root")).toHaveText(/asc-ui ready/);
  await page.addScriptTag({ content: bundles.submit ?? "", type: "module" });
  await page.waitForFunction(() => typeof window.ascStartSubmit === "function");
  await page.evaluate(([c, a, f]) => window.ascStartSubmit(c as never, a as string[], f as string | undefined), [changes, ["ann", "bob"], fail]);
}

test("submit: options become one run and edited PR text is patched", async ({ page }) => {
  await startSubmit(page);
  await page.getByTestId("su-mode").selectOption("publish");
  await expect(page.getByTestId("su-add-reviewer").locator("option")).toHaveCount(3);
  await page.getByTestId("su-add-reviewer").selectOption("ann");
  await expect(page.getByTestId("su-reviewer-ann")).toBeVisible();
  await page.getByTestId("su-auto").check();
  await expect(page.getByTestId("su-title-bbbb")).toHaveValue("Second change");
  await page.getByTestId("su-title-aaaa").fill("Better title");
  await page.locator('[data-action="submit"]').click();
  await expect.poll(() => page.evaluate(() => window.ascSubmit.runs.length)).toBe(1);
  expect(await page.evaluate(() => window.ascSubmit.runs[0])).toEqual({
    verb: "submit",
    args: ["-s", "--publish", "--reviewer=ann", "--auto-merge"],
  });
  expect(await page.evaluate(() => window.ascSubmit.edits)).toEqual([
    { change: "aaaa", number: 7, patch: { title: "Better title" } },
  ]);
});

test("submit: a draft cannot auto-merge and says why", async ({ page }) => {
  await startSubmit(page);
  await page.getByTestId("su-mode").selectOption("draft");
  await page.getByTestId("su-auto").check();
  await expect(page.locator('[data-action="submit"]')).toBeDisabled();
  await expect(page.locator('[data-action="submit"]')).toHaveAttribute("title", "A draft cannot auto-merge");
});

test("submit: a failed run reports the error and edits no PR", async ({ page }) => {
  await startSubmit(page, "server refused");
  await page.getByTestId("su-title-aaaa").fill("Changed");
  await page.locator('[data-action="submit"]').click();
  await expect(page.getByTestId("su-error")).toHaveText("server refused");
  expect(await page.evaluate(() => window.ascSubmit.edits)).toEqual([]);
});
