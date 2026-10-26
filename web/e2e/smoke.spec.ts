// Smoke suite: every route of docs/WEB-UI.md section 3, in light and dark, at the desktop (1440) and
// mobile (390) projects of playwright.config.ts. Each page must load with no console error or
// warning, no failed or 4xx/5xx request (a CSP violation is a console error), no horizontal page
// scroll, one h1, and no axe-core violation at WCAG 2.2 AA plus best practice.
import { expect, test } from "@playwright/test";
import { axeViolations, watch } from "./helpers";

const shots = process.env.ASC_E2E_SHOTS;

/** The demo repository of the mock transport (src/api/fixtures.ts). */
const R = "/ui/asc/demo";
const ROUTES: Record<string, string> = {
  landing: "/ui/",
  signin: "/ui/signin",
  settings: "/ui/settings",
  inbox: "/ui/inbox",
  search: "/ui/search",
  "new-repo": "/ui/new",
  repo: R,
  tree: `${R}/tree/main/src`,
  blob: `${R}/blob/main/src/index.ts`,
  "blob-big": `${R}/blob/main/data/big.txt`,
  blame: `${R}/blame/main/src/index.ts`,
  history: `${R}/commits/main`,
  commit: `${R}/commit/d69a79a966`,
  changes: `${R}/changes`,
  review: `${R}/changes/c1`,
  actions: `${R}/actions`,
  run: `${R}/actions/runs/1`,
  "repo-search": `${R}/search`,
  "repo-settings": `${R}/settings`,
  "not-found": "/ui/no/such/page/at/all",
};

for (const scheme of ["light", "dark"] as const) {
  test.describe(scheme, () => {
    test.use({ colorScheme: scheme });
    for (const [name, path] of Object.entries(ROUTES)) {
      test(`${name} renders cleanly`, async ({ page }, info) => {
        const problems = watch(page);
        await page.goto(path, { waitUntil: "networkidle" });
        await expect(page.locator("main h1")).toHaveCount(1);
        const dark = await page.evaluate(() => document.documentElement.classList.contains("dark"));
        expect(dark).toBe(scheme === "dark");
        // Loading states are finished: no busy region is left.
        await expect(page.locator("[aria-busy='true']")).toHaveCount(0);
        const overflow = await page.evaluate(
          () => document.documentElement.scrollWidth - document.documentElement.clientWidth,
        );
        expect(overflow, "horizontal page scroll").toBeLessThanOrEqual(0);
        expect(await axeViolations(page), "axe violations").toEqual([]);
        if (shots) {
          await page.screenshot({ path: `${shots}/${name}-${info.project.name}-${scheme}.png` });
        }
        expect(problems).toEqual([]);
      });
    }
  });
}

interface ClsWindow {
  clsTotal: number;
}

test.describe("interaction", () => {
  test("the page does not shift while data arrives (CLS)", async ({ page }) => {
    await page.addInitScript(() => {
      (window as unknown as ClsWindow).clsTotal = 0;
      new PerformanceObserver((list) => {
        for (const e of list.getEntries() as unknown as {
          value: number;
          hadRecentInput: boolean;
        }[]) {
          if (!e.hadRecentInput) (window as unknown as ClsWindow).clsTotal += e.value;
        }
      }).observe({ type: "layout-shift", buffered: true });
    });
    await page.goto(R, { waitUntil: "networkidle" });
    await page.waitForTimeout(300);
    const cls = await page.evaluate(() => (window as unknown as ClsWindow).clsTotal);
    expect(cls).toBeLessThan(0.02);
  });

  test("repo home to a file and back, with the file tree and README", async ({ page }) => {
    await page.goto(R);
    await expect(page.getByRole("link", { name: "package.json" })).toBeVisible();
    await expect(page.getByRole("heading", { name: "Demo" })).toBeVisible();
    await page.getByRole("link", { name: "src", exact: true }).click();
    await expect(page).toHaveURL(/\/tree\/main\/src$/);
    await page.getByRole("link", { name: "index.ts" }).click();
    await expect(page).toHaveURL(/\/blob\/main\/src\/index\.ts$/);
    await expect(page.getByText("export { hello }")).toBeVisible();
    await page.goBack();
    await expect(page).toHaveURL(/\/tree\/main\/src$/);
  });

  test("a 50,000-line file keeps the DOM small", async ({ page }) => {
    await page.goto(`${R}/blob/main/data/big.txt`, { waitUntil: "networkidle" });
    await expect(page.getByText("line 1:").first()).toBeVisible();
    expect(await page.locator("a[id^='L']").count()).toBeLessThan(300);
  });

  test("keyboard: ? opens help, Escape closes it, g then p goes to changes", async ({
    page,
    isMobile,
  }) => {
    test.skip(isMobile, "no hardware keyboard");
    await page.goto(R, { waitUntil: "networkidle" });
    await page.keyboard.press("?");
    await expect(page.getByRole("dialog", { name: "Keyboard shortcuts" })).toBeVisible();
    await page.keyboard.press("Escape");
    await expect(page.getByRole("dialog")).toHaveCount(0);
    await page.keyboard.press("g");
    await page.keyboard.press("p");
    await expect(page).toHaveURL(/\/asc\/demo\/changes$/);
  });

  test("keyboard: / focuses search and typing in it triggers no shortcut", async ({
    page,
    isMobile,
  }) => {
    test.skip(isMobile, "no hardware keyboard");
    await page.goto("/ui/", { waitUntil: "networkidle" });
    await page.keyboard.press("/");
    await expect(page.getByRole("searchbox")).toBeFocused();
    await page.keyboard.type("g?");
    await expect(page.getByRole("dialog")).toHaveCount(0);
    await expect(page).toHaveURL(/\/ui\/$/);
  });

  test("skip link moves focus to the main landmark", async ({ page, isMobile }) => {
    test.skip(isMobile, "no hardware keyboard");
    await page.goto("/ui/", { waitUntil: "networkidle" });
    await page.keyboard.press("Tab");
    const skip = page.getByRole("link", { name: "Skip to content" });
    await expect(skip).toBeFocused();
    await expect(skip).toBeVisible();
    await page.keyboard.press("Enter");
    await expect(page.locator("#main")).toBeFocused();
  });

  test("theme choice persists across a reload and applies before paint", async ({ page }) => {
    await page.goto("/ui/settings", { waitUntil: "networkidle" });
    await page.getByLabel("Dark").check();
    await expect(page.locator("html")).toHaveClass(/dark/);
    await page.reload({ waitUntil: "networkidle" });
    await expect(page.locator("html")).toHaveClass(/dark/);
    await page.getByLabel("Light").check();
    await page.reload({ waitUntil: "networkidle" });
    await expect(page.locator("html")).not.toHaveClass(/dark/);
  });

  test("sign-in keeps the token out of the URL and local storage", async ({ page }) => {
    await page.goto("/ui/signin", { waitUntil: "networkidle" });
    await page.getByLabel("Token").fill("secret-token-value");
    await page.getByRole("button", { name: "Sign in" }).click();
    await expect(page).toHaveURL(/\/ui\/?$/);
    expect(page.url()).not.toContain("secret-token-value");
    const stored = await page.evaluate(() => JSON.stringify({ ...localStorage }));
    expect(stored).not.toContain("secret-token-value");
    await expect(page.getByRole("button", { name: "Sign out" })).toBeVisible();
  });

  test("the served page has the strict CSP and no inline script", async ({ page }) => {
    const res = await page.goto("/ui/", { waitUntil: "networkidle" });
    const csp = res?.headers()["content-security-policy"] ?? "";
    expect(csp).toContain("script-src 'self'");
    expect(csp).not.toContain("unsafe-inline");
    expect(csp).not.toContain("unsafe-eval");
    const inline = await page.evaluate(
      () =>
        [...document.scripts].filter((s) => !s.src).length +
        document.querySelectorAll("[onclick],[onload]").length,
    );
    expect(inline).toBe(0);
  });

  test("an unknown file under /ui is a 404, not the app shell", async ({ request }) => {
    const res = await request.get("/ui/static/js/missing.js");
    expect(res.status()).toBe(404);
  });
});
