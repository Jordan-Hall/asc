// Visual regression suite. Every route is captured full-page at 1440 (desktop project) and 390
// (mobile project), in light and dark, and compared with the stored baseline in
// e2e/visual.spec.ts-snapshots/. The baselines were accepted after comparing each route with its
// board in the design project (docs/designs/web-boards.md lists route -> board).
//
// Update after an intended visual change:  bun run e2e e2e/visual.spec.ts --update-snapshots
// Dump the captures for a side-by-side with the boards:  ASC_E2E_SHOTS=<dir> bun run e2e e2e/visual.spec.ts
import { expect, test, type Page } from "@playwright/test";
import { VISUAL_ROUTES } from "./helpers";

const dump = process.env.ASC_E2E_SHOTS;

/** Freeze everything that moves, so two captures of one route are the same pixels. */
async function settle(page: Page): Promise<void> {
  await expect(page.locator("[aria-busy='true']")).toHaveCount(0);
  await page.evaluate(() => document.fonts.ready);
}

for (const scheme of ["light", "dark"] as const) {
  test.describe(`visual ${scheme}`, () => {
    test.use({ colorScheme: scheme });
    for (const route of VISUAL_ROUTES) {
      test(`${route.name}`, async ({ page }, info) => {
        if (route.signedIn) {
          await page.addInitScript(() => sessionStorage.setItem("asc-token", "demo-token"));
        }
        await page.goto(route.path, { waitUntil: "networkidle" });
        await settle(page);
        if (dump) {
          await page.screenshot({
            path: `${dump}/${route.name}.${info.project.name}.${scheme}.png`,
            fullPage: true,
          });
          if (process.env.ASC_E2E_DUMP_ONLY === "1") return;
        }
        await expect(page).toHaveScreenshot(`${route.name}-${scheme}.png`, {
          fullPage: true,
          animations: "disabled",
          maxDiffPixelRatio: 0.002,
        });
      });
    }
  });
}
