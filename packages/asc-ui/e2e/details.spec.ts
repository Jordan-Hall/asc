import { execFileSync } from "node:child_process";
import { mkdtempSync, readFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { expect, type Page, test } from "@playwright/test";

const entry = join(import.meta.dirname, "../src/views/details/e2e-entry.ts");
let bundle = "";

test.beforeAll(() => {
  const out = join(mkdtempSync(join(tmpdir(), "asc-details-")), "details.js");
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

const person = { name: "Amy <b>x</b>", email: "amy@example.com", time: 1_700_000_000 };
const details = {
  id: "abcdef0123456789",
  parents: ["0123456789abcdef"],
  message: "Fix the parser\n\nLonger body\nsecond line <img src=x onerror=alert(1)>\n",
  author: person,
  signature: "G",
  change: "kxyzkxyzkxyz",
  review: {
    forge: "forgejo",
    number: 42,
    url: "https://forge.example/o/r/pulls/42",
    state: "open",
    draft: false,
  },
  versions: [
    { v: 2, rev: "bbbb00000000", at: 1_700_000_100, actor: "amy" },
    { v: 1, rev: "aaaa00000000", at: 1_700_000_000, actor: "amy" },
  ],
  files: [
    { path: "src/z.ts", status: "modified", added: 3, removed: 1 },
    { path: "src/a.ts", status: "added", added: 10, removed: 0 },
    { path: "src/new.ts", oldPath: "src/old.ts", status: "renamed", added: 0, removed: 0 },
    { path: "img.png", status: "added", binary: true },
  ],
};
const compare = {
  a: { kind: "revision", value: "main" },
  b: { kind: "revision", value: "topic" },
  mergeBase: "0123456789abcdef",
  ahead: 2,
  behind: 1,
  aheadEntries: [
    { id: "c2", subject: "two", author: "amy", time: 2 },
    { id: "c1", subject: "one", author: "amy", time: 1 },
  ],
  behindEntries: [{ id: "d1", subject: "other", author: "bob", time: 1 }],
  files: [{ path: "src/a.ts", status: "modified" }],
  aheadMore: false,
  behindMore: false,
};

async function start(page: Page): Promise<void> {
  await page.goto("/");
  await expect(page.locator("#root")).toHaveText(/asc-ui ready/);
  await page.addScriptTag({ content: bundle, type: "module" });
  await page.waitForFunction(() => typeof window.ascDetailsStart === "function");
  await page.evaluate((d) => window.ascDetailsStart(d as never), { details, compare });
}

test("details show subject, body, change, signature and totals; markup stays text", async ({
  page,
}) => {
  await start(page);
  await expect(page.getByTestId("subject")).toHaveText("Fix the parser");
  await expect(page.getByTestId("body")).toContainText("<img src=x onerror=alert(1)>");
  await expect(page.locator("[data-testid=details] img")).toHaveCount(0);
  await expect(page.getByTestId("author")).toContainText("<b>x</b>");
  await expect(page.getByTestId("signed")).toBeVisible();
  await expect(page.getByTestId("change")).toContainText("kxyzkxyzkxyz");
  await expect(page.getByTestId("files-head").first()).toHaveText("4 files changed, +13 -1");
});

test("files open their diff, rename shows both paths, all changes opens the multi-diff", async ({
  page,
}) => {
  await start(page);
  await expect(page.getByTestId("file-src/new.ts")).toContainText("src/old.ts → src/new.ts");
  await expect(page.getByTestId("file-img.png")).toContainText("binary");
  await page.getByTestId("file-src/a.ts").first().dblclick();
  await page.locator("[data-testid=details] [data-action=open-all]").click();
  const c = await page.evaluate(() => window.ascDetailsCalls);
  expect(c.diffs).toEqual(["src/a.ts"]);
  expect(c.all).toEqual(["abcdef0123456789"]);
});

test("PR, parent and version links go through the host", async ({ page }) => {
  await start(page);
  await page.locator("[data-action=open-review]").click();
  await page.locator("[data-action=parent]").click();
  await page.getByTestId("version-1").dblclick();
  const c = await page.evaluate(() => window.ascDetailsCalls);
  expect(c.reviews).toEqual(["https://forge.example/o/r/pulls/42"]);
  expect(c.revs).toEqual(["0123456789abcdef", "aaaa00000000"]);
});

test("compare needs both sides, then renders ahead, behind and files", async ({ page }) => {
  await start(page);
  await page.locator("[data-testid=compare] [data-action=compare]").click();
  await expect(page.getByTestId("status")).toHaveText("Pick both sides");
  await page.getByTestId("a-value").fill("main");
  await page.getByTestId("b-kind").selectOption("stack");
  await page.getByTestId("b-value").fill("topic");
  await page.getByTestId("b-value").press("Enter");
  await expect(page.getByTestId("status")).toHaveText("stack topic vs main: 2 ahead, 1 behind");
  await expect(page.getByTestId("ahead-c2")).toBeVisible();
  await expect(page.getByTestId("behind-d1")).toBeVisible();
  const c = await page.evaluate(() => window.ascDetailsCalls.compares);
  expect(c).toEqual([
    { a: { kind: "revision", value: "main" }, b: { kind: "stack", value: "topic" } },
  ]);
});
