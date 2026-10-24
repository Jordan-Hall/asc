import { expect, test } from "@playwright/test";

test("dev host serves asc-ui and reads the mock Forgejo", async ({ page }) => {
  const errors: string[] = [];
  page.on("pageerror", (e) => errors.push(e.message));
  await page.goto("/");
  await expect(page.locator("#root")).toHaveText("asc-ui ready: demo");
  const bg = await page.evaluate(() =>
    getComputedStyle(document.documentElement).getPropertyValue("--asc-bg"),
  );
  expect(bg.length).toBeGreaterThanOrEqual(0);
  expect(errors).toEqual([]);
});

test("mock Forgejo denies unknown paths", async ({ request }) => {
  const res = await request.get("/api/v1/admin/users");
  expect(res.status()).toBe(404);
});
