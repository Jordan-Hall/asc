// Playwright harness (SPEC §24.7.1): Chromium against the asc-ui dev host. The dev host is started
// by the config's webServer on a loopback port; specs live in e2e/.
import { defineConfig } from "@playwright/test";

const port = 4173;

export default defineConfig({
  testDir: "./e2e",
  testMatch: /.*\.spec\.ts/,
  fullyParallel: true,
  reporter: [["list"]],
  use: { baseURL: `http://127.0.0.1:${port}`, trace: "retain-on-failure" },
  projects: [{ name: "chromium", use: { browserName: "chromium" } }],
  webServer: {
    command: "bun ../asc-ui-devhost/src/main.ts",
    url: `http://127.0.0.1:${port}/`,
    env: { PORT: String(port) },
    reuseExistingServer: false,
    timeout: 30_000,
  },
});
