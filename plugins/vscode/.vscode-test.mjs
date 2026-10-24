// test-electron harness (`bun run test:e2e`): runs test/e2e/** inside a real VS Code. The unit
// tests under test/ run in Bun without it. No proposed APIs are enabled.
import { defineConfig } from "@vscode/test-cli";

export default defineConfig({
  files: "test/e2e/**/*.test.cjs",
  version: "1.101.0",
  launchArgs: ["--disable-extensions", "--disable-workspace-trust"],
  mocha: { ui: "tdd", timeout: 20000 },
});
