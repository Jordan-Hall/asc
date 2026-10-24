import { execFileSync } from "node:child_process";
import { mkdtempSync, readFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { expect, type Page, test } from "@playwright/test";

const entry = join(import.meta.dirname, "../src/views/inbox/e2e-entry.ts");
let bundle = "";

test.beforeAll(() => {
  const out = join(mkdtempSync(join(tmpdir(), "asc-inbox-")), "inbox.js");
  // shell: bun is a .cmd shim on Windows; the arguments are fixed paths, never user input.
  execFileSync("bun", ["build", `"${entry}"`, "--target=browser", "--format=esm", "--outfile", `"${out}"`], {
    stdio: "pipe",
    shell: true,
  });
  bundle = readFileSync(out, "utf8");
});

const now = Date.parse("2026-01-01T00:00:00Z");
const pr = (number: number, title: string) => ({ number, title });
const seed = [
  { id: 1, section: "needs_review", pull_request: pr(1, "Add stack view") },
  { id: 2, section: "needs_review", pull_request: pr(2, "Fix restack") },
  { id: 3, section: "ready", pinned: true, pull_request: pr(3, "Ready one") },
  { id: 4, section: "drafts", snoozed_until: "2026-02-01T00:00:00Z", pull_request: pr(4, "Snoozed draft") },
];

async function start(page: Page): Promise<void> {
  await page.goto("/");
  await expect(page.locator("#root")).toHaveText(/asc-ui ready/);
  await page.addScriptTag({ content: bundle, type: "module" });
  await page.waitForFunction(() => typeof window.ascStartInbox === "function");
  await page.evaluate(([d, n]) => window.ascStartInbox(d as never, n as number), [seed, now] as const);
}

test("sections show entries; snoozed hidden until toggled", async ({ page }) => {
  await start(page);
  await expect(page.getByTestId("section-needs_review").locator("li")).toHaveCount(2);
  await expect(page.getByTestId("section-ready").locator('[data-badge="pinned"]')).toBeVisible();
  await expect(page.getByTestId("item-4")).toHaveCount(0);
  await page.getByLabel("Show snoozed").check();
  await expect(page.getByTestId("item-4").locator('[data-badge="snoozed"]')).toBeVisible();
});

test("pin and snooze go through the inbox API", async ({ page }) => {
  await start(page);
  await page.getByTestId("item-1").locator('[data-action="pin"]').click();
  await expect(page.getByTestId("item-1").locator('[data-badge="pinned"]')).toBeVisible();
  await page.getByTestId("item-2").locator('[data-action="snooze"]').selectOption("1h");
  await expect(page.getByTestId("item-2")).toHaveCount(0);
  const calls = await page.evaluate(() => window.ascInbox.calls.updates);
  expect(calls).toEqual([
    { id: 1, body: { pinned: true } },
    { id: 2, body: { snoozed_until: "2026-01-01T01:00:00.000Z" } },
  ]);
});

test("clicking a title opens the pull request", async ({ page }) => {
  await start(page);
  await page.getByTestId("item-3").locator(".asc-inbox-title").click();
  expect(await page.evaluate(() => window.ascInbox.calls.opened)).toEqual([3]);
});
