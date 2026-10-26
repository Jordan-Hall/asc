// Shared checks for the browser suites: console and network problems, and axe-core.
import { readFileSync } from "node:fs";
import { createRequire } from "node:module";
import type { Page } from "@playwright/test";

const axeSource = readFileSync(
  createRequire(import.meta.url).resolve("axe-core/axe.min.js"),
  "utf8",
);

/** Collects everything that should never happen on a healthy page. */
export function watch(page: Page): string[] {
  const problems: string[] = [];
  page.on("console", (m) => {
    if (m.type() === "error" || m.type() === "warning") {
      problems.push(`console ${m.type()}: ${m.text()}`);
    }
  });
  page.on("pageerror", (e) => problems.push(`pageerror: ${e.message}`));
  page.on("requestfailed", (r) => problems.push(`request failed: ${r.url()}`));
  page.on("response", (r) => {
    if (r.status() >= 400) problems.push(`HTTP ${r.status()}: ${r.url()}`);
  });
  return problems;
}

interface AxeViolation {
  id: string;
  nodes: { target: unknown[] }[];
}
interface AxeGlobal {
  axe: { run(ctx: Document, o: object): Promise<{ violations: AxeViolation[] }> };
}

export async function axeViolations(page: Page): Promise<string[]> {
  // evaluate() runs through the debugger protocol, so the strict CSP does not block it.
  await page.evaluate(axeSource);
  return page.evaluate(async () => {
    const r = await (globalThis as unknown as AxeGlobal).axe.run(document, {
      runOnly: ["wcag2a", "wcag2aa", "wcag21a", "wcag21aa", "wcag22aa", "best-practice"],
    });
    return r.violations.map((v) => `${v.id} x${v.nodes.length} at ${v.nodes[0]?.target.join(" ")}`);
  });
}

/** The demo repository of the mock transport (src/api/fixtures.ts). */
export const R = "/ui/asc/demo";
/** Routes captured by the visual suite; `signedIn` sets a session token before load. */
export const VISUAL_ROUTES: { name: string; path: string; signedIn: boolean }[] = [
  { name: "landing", path: "/ui/", signedIn: false },
  { name: "dashboard", path: "/ui/", signedIn: true },
  { name: "signin", path: "/ui/signin", signedIn: false },
  { name: "settings", path: "/ui/settings", signedIn: true },
  { name: "inbox", path: "/ui/inbox", signedIn: true },
  { name: "search", path: "/ui/search", signedIn: true },
  { name: "repo", path: "/ui/asc/demo", signedIn: true },
  { name: "tree", path: "/ui/asc/demo/tree/main/src", signedIn: true },
  { name: "blob", path: "/ui/asc/demo/blob/main/src/index.ts", signedIn: true },
  { name: "changes", path: "/ui/asc/demo/changes", signedIn: true },
  { name: "review", path: "/ui/asc/demo/changes/c1", signedIn: true },
];

export const ROUTES: Record<string, string> = {
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
