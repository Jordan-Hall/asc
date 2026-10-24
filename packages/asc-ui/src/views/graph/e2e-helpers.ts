import { execFileSync } from "node:child_process";
import { mkdtempSync, readFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { expect, type Page } from "@playwright/test";

const entry = join(import.meta.dirname, "e2e-entry.ts");

/** Bundle the graph e2e entry once per spec file. */
export function buildGraphBundle(): string {
  const out = join(mkdtempSync(join(tmpdir(), "asc-graph-")), "graph.js");
  // shell: bun is a .cmd shim on Windows; the arguments are fixed paths, never user input.
  execFileSync("bun", ["build", `"${entry}"`, "--target=browser", "--format=esm", "--outfile", `"${out}"`], {
    stdio: "pipe",
    shell: true,
  });
  return readFileSync(out, "utf8");
}

export async function startGraph(
  page: Page,
  bundle: string,
  opts: { total: number; omitTotal?: boolean; delayMs?: number },
): Promise<void> {
  await page.goto("/");
  await expect(page.locator("#root")).toHaveText(/asc-ui ready/);
  await page.addScriptTag({ content: bundle, type: "module" });
  await page.waitForFunction(() => typeof window.ascStartGraph === "function");
  await page.evaluate((o) => window.ascStartGraph(o), opts);
}
