// Enforces the performance budgets of docs/WEB-UI.md section 6 against dist/manifest.json.
// All numbers are gzip bytes. Exit status 1 on any violation.
import { readFileSync } from "node:fs";

const KB = 1024;
export const BUDGET = {
  initialJs: 150 * KB,
  initialCss: 20 * KB,
  /** Any chunk the entry does not load up front. Route chunks should stay under 40 KB. */
  asyncChunk: 80 * KB,
  routeChunkWarn: 40 * KB,
};

interface FileInfo {
  path: string;
  size: number;
  gz?: number;
}

const dist = new URL("../dist", import.meta.url).pathname.replace(/^\/([A-Za-z]:)/, "$1");
const manifest = JSON.parse(readFileSync(`${dist}/manifest.json`, "utf8")) as { files: FileInfo[] };
const html = readFileSync(`${dist}/index.html`, "utf8");
const byPath = new Map(manifest.files.map((f) => [f.path, f]));

function referenced(re: RegExp): string[] {
  return [...html.matchAll(re)].map((m) => (m[1] ?? "").replace(/^\/ui\//, ""));
}
const gz = (p: string) => byPath.get(p)?.gz ?? byPath.get(p)?.size ?? 0;

const initialJs = referenced(/<script[^>]*\ssrc="([^"]+)"/g).filter((p) => p.startsWith("static/"));
const initialCss = [...html.matchAll(/<link[^>]*>/g)]
  .map((m) => m[0])
  .filter((tag) => /rel="stylesheet"/.test(tag))
  .map((tag) => (/href="([^"]+)"/.exec(tag)?.[1] ?? "").replace(/^\/ui\//, ""));
const initialSet = new Set([...initialJs, ...initialCss]);

const sum = (paths: string[]) => paths.reduce((n, p) => n + gz(p), 0);
const jsTotal = sum(initialJs);
const cssTotal = sum(initialCss);
const asyncJs = manifest.files.filter(
  (f) => f.path.startsWith("static/js/") && f.path.endsWith(".js") && !initialSet.has(f.path),
);

const fmt = (n: number) => `${(n / KB).toFixed(1)} KB`;
const lines: string[] = [];
const fail: string[] = [];

lines.push(
  `initial JS   ${fmt(jsTotal).padStart(10)} gz  (budget ${fmt(BUDGET.initialJs)})  ${initialJs.length} files`,
);
for (const p of initialJs) lines.push(`  ${fmt(gz(p)).padStart(10)}  ${p}`);
lines.push(`initial CSS  ${fmt(cssTotal).padStart(10)} gz  (budget ${fmt(BUDGET.initialCss)})`);
lines.push(`async chunks ${asyncJs.length}`);
for (const f of asyncJs.toSorted((a, b) => gz(b.path) - gz(a.path))) {
  const warn = gz(f.path) > BUDGET.routeChunkWarn ? "  (over 40 KB)" : "";
  lines.push(`  ${fmt(gz(f.path)).padStart(10)}  ${f.path}${warn}`);
  if (gz(f.path) > BUDGET.asyncChunk)
    fail.push(`${f.path} is ${fmt(gz(f.path))}, over ${fmt(BUDGET.asyncChunk)}`);
}
if (jsTotal > BUDGET.initialJs)
  fail.push(`initial JS ${fmt(jsTotal)} is over ${fmt(BUDGET.initialJs)}`);
if (cssTotal > BUDGET.initialCss)
  fail.push(`initial CSS ${fmt(cssTotal)} is over ${fmt(BUDGET.initialCss)}`);
if (initialJs.length === 0) fail.push("no initial script found in index.html");

process.stdout.write(`${lines.join("\n")}
`);
if (fail.length > 0) {
  process.stderr.write(`${`\nbudget exceeded:\n${fail.map((f) => `  ${f}`).join("\n")}`}
`);
  process.exit(1);
}
process.stdout.write(`${"\nsize: within budget"}
`);
