import { expect, test } from "@playwright/test";
import { buildGraphBundle, startGraph } from "../src/views/graph/e2e-helpers";

// SPEC §24.4.10: first 200 rows painted within 150 ms of the data, scrolling 100,000 rows at
// 55 fps or better, at most 5,000 rows in memory.
let bundle = "";
test.beforeAll(() => {
  bundle = buildGraphBundle();
});

test.describe.configure({ mode: "serial" });

test("first 200 rows paint within 150 ms of the data", async ({ page }) => {
  await startGraph(page, bundle, { total: 100_000 });
  await page.waitForFunction(() => window.ascGraph.paintMs >= 0);
  expect(await page.evaluate(() => window.ascGraph.paintMs)).toBeLessThanOrEqual(150);
});

test("scrolling 100,000 rows holds at least 55 fps with at most 5,000 rows in memory", async ({
  page,
}) => {
  await startGraph(page, bundle, { total: 100_000 });
  await page.waitForFunction(() => window.ascGraph.paintMs >= 0);
  const result = await page.evaluate(
    () =>
      new Promise<{ fps: number; held: number; max: number }>((resolve, reject) => {
        const s = document.querySelector<HTMLElement>('[data-testid="graph-scroll"]');
        if (!s) {
          reject(new Error("no scroller"));
          return;
        }
        const durationMs = 3000;
        const stepPx = 22 * 6; // six rows per frame, faster than any wheel
        let frames = 0;
        let maxHeld = 0;
        const t0 = performance.now();
        const tick = (now: number): void => {
          s.scrollTop += stepPx;
          frames++;
          maxHeld = Math.max(maxHeld, window.ascGraph.view.heldRows);
          if (now - t0 < durationMs) requestAnimationFrame(tick);
          else
            resolve({
              fps: (frames * 1000) / (now - t0),
              held: window.ascGraph.view.heldRows,
              max: maxHeld,
            });
        };
        requestAnimationFrame(tick);
      }),
  );
  expect(result.max).toBeLessThanOrEqual(5000);
  expect(result.held).toBeLessThanOrEqual(5000);
  expect(result.fps).toBeGreaterThanOrEqual(55);
});

test("a jump near the last row walks the page chain and respects the memory bound", async ({
  page,
}) => {
  await startGraph(page, bundle, { total: 100_000 });
  await page.waitForFunction(() => window.ascGraph.paintMs >= 0);
  await page.evaluate(() => {
    const s = document.querySelector<HTMLElement>('[data-testid="graph-scroll"]');
    if (s) s.scrollTop = 99_990 * 22;
  });
  await expect(page.locator('[data-index="99990"]')).toContainText("commit 99990", {
    timeout: 20_000,
  });
  await page.evaluate(() => window.ascGraph.view.idle());
  expect(await page.evaluate(() => window.ascGraph.view.heldRows)).toBeLessThanOrEqual(5000);
});
