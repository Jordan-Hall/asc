// Browser smoke suite for the web UI. It builds the mock-transport bundle and serves it with the
// production headers (e2e/serve.ts), then drives every route of docs/WEB-UI.md section 3.
// Chromium is picked up from PLAYWRIGHT_BROWSERS_PATH or the default Playwright cache.
// Run: bun run e2e   (ASC_E2E_SHOTS=<dir> also saves a screenshot per route and viewport)
import { defineConfig } from "@playwright/test";

const port = Number(process.env.ASC_E2E_PORT ?? 4180);

export default defineConfig({
  testDir: "./e2e",
  testMatch: /.*\.spec\.ts/,
  fullyParallel: true,
  forbidOnly: !!process.env.CI,
  retries: process.env.CI ? 1 : 0,
  reporter: [["list"]],
  use: {
    baseURL: `http://127.0.0.1:${port}`,
    trace: "retain-on-failure",
    contextOptions: { reducedMotion: "reduce" },
  },
  projects: [
    { name: "desktop", use: { browserName: "chromium", viewport: { width: 1440, height: 900 } } },
    {
      name: "mobile",
      use: {
        browserName: "chromium",
        viewport: { width: 390, height: 844 },
        isMobile: true,
        hasTouch: true,
      },
    },
  ],
  webServer: {
    command: "bun e2e/serve.ts",
    url: `http://127.0.0.1:${port}/ui/`,
    env: { PORT: String(port) },
    reuseExistingServer: !process.env.CI,
    timeout: 120_000,
  },
});
