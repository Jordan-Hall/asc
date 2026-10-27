// Mobile first: no route scrolls sideways at 320 or 390 px wide, in light and dark, signed out and
// signed in, with agent features off and on. Also the 44 px touch targets of the header and footer,
// and the command palette as a bottom sheet. Runs in both Playwright projects.
import { expect, type Page, test } from "@playwright/test";
import { ROUTES, watch } from "./helpers";

const WIDTHS = [320, 390] as const;

async function overflow(page: Page): Promise<{ page: number; body: number; wide: string[] }> {
  return page.evaluate(() => {
    const root = document.documentElement;
    const limit = root.clientWidth;
    // Elements that stick out past the right edge and are not inside a scrolling box.
    const wide: string[] = [];
    for (const el of document.body.querySelectorAll<HTMLElement>("*")) {
      const r = el.getBoundingClientRect();
      if (r.width === 0 || r.right <= limit + 1) continue;
      let scrolls = false;
      for (let p = el.parentElement; p && p !== document.body; p = p.parentElement) {
        const o = getComputedStyle(p).overflowX;
        if (o === "auto" || o === "scroll" || o === "hidden" || o === "clip") {
          scrolls = true;
          break;
        }
      }
      const fixed = getComputedStyle(el).position === "fixed";
      if (!scrolls && !fixed && wide.length < 5) {
        wide.push(
          `${el.tagName.toLowerCase()}.${String(el.className).slice(0, 60)} right=${Math.round(r.right)}`,
        );
      }
    }
    return { page: root.scrollWidth - limit, body: document.body.scrollWidth - limit, wide };
  });
}

async function signedIn(page: Page): Promise<void> {
  await page.addInitScript(() => {
    sessionStorage.setItem("asc-token", "demo-token");
    localStorage.setItem(
      "asc-recent",
      JSON.stringify(["acme/payments", "asc/demo", "acme/web", "acme/ledger", "asc/engine"]),
    );
  });
}

for (const width of WIDTHS) {
  for (const scheme of ["light", "dark"] as const) {
    test.describe(`${width}px, ${scheme}`, () => {
      test.use({ colorScheme: scheme, viewport: { width, height: 800 } });

      for (const [name, path] of Object.entries(ROUTES)) {
        test(`${name}: no horizontal scroll`, async ({ page }) => {
          const problems = watch(page);
          await page.goto(path, { waitUntil: "networkidle" });
          await expect(page.locator("[aria-busy='true']")).toHaveCount(0);
          await page.waitForTimeout(150);
          const o = await overflow(page);
          expect(o.page, "page scrollWidth over clientWidth").toBeLessThanOrEqual(0);
          expect(o.body, "body scrollWidth over clientWidth").toBeLessThanOrEqual(0);
          expect(o.wide, "elements past the right edge").toEqual([]);
          expect(problems).toEqual([]);
        });

        test(`${name}: signed in, no horizontal scroll`, async ({ page }) => {
          await signedIn(page);
          await page.goto(path, { waitUntil: "networkidle" });
          await expect(page.locator("[aria-busy='true']")).toHaveCount(0);
          await page.waitForTimeout(150);
          const o = await overflow(page);
          expect(o.page).toBeLessThanOrEqual(0);
          expect(o.wide).toEqual([]);
        });
      }

      test("dashboard with agent features on", async ({ page }) => {
        await signedIn(page);
        await page.addInitScript(() => localStorage.setItem("asc-agents", "1"));
        await page.goto("/ui/", { waitUntil: "networkidle" });
        await expect(page.getByRole("region", { name: "Needs you" })).toBeVisible();
        expect((await overflow(page)).page).toBeLessThanOrEqual(0);
      });
    });
  }
}

test.describe("touch targets and the palette sheet", () => {
  test.use({ viewport: { width: 320, height: 700 }, hasTouch: true, isMobile: true });

  const small = (page: Page, scope: string) =>
    page.evaluate((sel) => {
      const out: string[] = [];
      for (const el of document.querySelectorAll<HTMLElement>(
        `${sel} a, ${sel} button, ${sel} input, ${sel} [role='radio'], ${sel} [role='switch']`,
      )) {
        const r = el.getBoundingClientRect();
        if (r.width === 0 || r.height === 0) continue;
        // The skip link is off screen until focused.
        if (el.classList.contains("sr-only-focusable")) continue;
        if (r.height < 43.5 || r.width < 43.5) {
          out.push(
            `${el.tagName.toLowerCase()} "${(el.getAttribute("aria-label") ?? el.textContent ?? "").trim().slice(0, 30)}" ${Math.round(r.width)}x${Math.round(r.height)}`,
          );
        }
      }
      return out;
    }, scope);

  for (const signed of [false, true]) {
    test(`header and footer controls are 44 px, ${signed ? "signed in" : "signed out"}`, async ({
      page,
    }) => {
      if (signed) await signedIn(page);
      await page.goto("/ui/", { waitUntil: "networkidle" });
      expect(await small(page, "header")).toEqual([]);
      expect(await small(page, "footer")).toEqual([]);
    });
  }

  test("the command palette is a bottom sheet that fits the screen", async ({ page }) => {
    await signedIn(page);
    await page.goto("/ui/", { waitUntil: "networkidle" });
    await page.getByRole("button", { name: "Open menu" }).click();
    await page.getByRole("button", { name: "Search or run a command" }).click();
    const dialog = page.getByRole("dialog", { name: "Command palette" });
    await expect(dialog).toBeVisible();
    await page.waitForTimeout(250);
    const box = await dialog.boundingBox();
    const vp = page.viewportSize();
    expect(box && vp).toBeTruthy();
    if (box && vp) {
      expect(box.x).toBeGreaterThanOrEqual(0);
      expect(box.x + box.width).toBeLessThanOrEqual(vp.width);
      expect(Math.round(box.y + box.height)).toBe(vp.height);
      expect(box.width).toBe(vp.width);
    }
    expect(await small(page, "dialog[open]")).toEqual([]);
    await page.getByRole("button", { name: "Close" }).click();
    await expect(dialog).toHaveCount(0);
  });

  test("the agent features switch in Settings shows the agent runs page", async ({ page }) => {
    await signedIn(page);
    await page.goto("/ui/agents", { waitUntil: "networkidle" });
    await expect(page.getByText("Agent features are off in this browser.")).toBeVisible();
    await page.goto("/ui/settings", { waitUntil: "networkidle" });
    await page.getByRole("checkbox", { name: "Show agent features" }).check();
    await page.goto("/ui/agents", { waitUntil: "networkidle" });
    await expect(page.getByRole("heading", { level: 1, name: "Agent runs" })).toBeVisible();
    await expect(page.getByText("Add retry policy")).toBeVisible();
    await page.goto("/ui/settings", { waitUntil: "networkidle" });
    await page.getByRole("checkbox", { name: "Show agent features" }).uncheck();
  });
});
