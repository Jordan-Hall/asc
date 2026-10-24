// Plugin budget gate (SPEC 24.4.10, P44a). `bun bench/plugins/collect.ts --target vscode [--check]
// [--update] [--build] [--require-host] [--graph]`.
//
//  - bundle sizes: always measured from plugins/vscode/dist (host min bytes, asc-ui gzip bytes).
//  - bridge RSS: spawns `asc api --stdio` (ASC_BIN or PATH) idle and reads the process RSS.
//  - activation and attributable heap: a test-only extension-host window (heap-probe/) is started
//    with --disable-extensions --extensionDevelopmentPath and --js-flags=--expose-gc, once on an asc
//    fixture and once on a non-asc folder (the extension inert); heap is the delta, median of 5.
//    Needs VS Code (ASC_VSCODE_PATH, or `code` on PATH) and a display; otherwise the metrics are
//    reported "unmeasured" and skipped, unless --require-host makes that a failure.
//  - graph: --graph runs the Playwright graph-perf spec (it asserts 150 ms / 55 fps / 5,000 rows).
// --check fails on an absolute budget breach or a regression over baseline.vscode.json; --update
// rewrites the baseline from this run. No secrets are read or logged; nothing is sent anywhere.
import { spawn, spawnSync } from "node:child_process";
import { existsSync, mkdirSync, mkdtempSync, readFileSync, readdirSync, rmSync, statSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";
import { gzipSync } from "node:zlib";

const here = dirname(fileURLToPath(import.meta.url));
const root = resolve(here, "..", "..");
const KIB = 1024;
const MIB = 1024 * KIB;

export type Unit = "ms" | "bytes" | "fps" | "bool";
export interface MetricDef {
  name: string;
  unit: Unit;
  /** "max": value must be <= budget. "min": value must be >= budget. */
  dir: "max" | "min";
  budget: number;
  /** Regression slack over baseline: relative fraction and absolute floor. */
  rel: number;
  abs: number;
}

export const VSCODE_METRICS: readonly MetricDef[] = [
  { name: "activateMs", unit: "ms", dir: "max", budget: 30, rel: 0.5, abs: 10 },
  { name: "nonAscCostMs", unit: "ms", dir: "max", budget: 5, rel: 0.5, abs: 3 },
  { name: "heapDeltaBytes", unit: "bytes", dir: "max", budget: 25 * MIB, rel: 0.15, abs: 1 * MIB },
  { name: "bridgeRssBytes", unit: "bytes", dir: "max", budget: 20 * MIB, rel: 0.15, abs: 1 * MIB },
  { name: "hostBundleBytes", unit: "bytes", dir: "max", budget: 350 * KIB, rel: 0.05, abs: 2 * KIB },
  { name: "uiInitialGzipBytes", unit: "bytes", dir: "max", budget: 300 * KIB, rel: 0.05, abs: 2 * KIB },
  { name: "uiLazyMaxGzipBytes", unit: "bytes", dir: "max", budget: 150 * KIB, rel: 0.05, abs: 2 * KIB },
  { name: "graphPerfPass", unit: "bool", dir: "min", budget: 1, rel: 0, abs: 0 },
];

export type Measured = Record<string, number | null>;
export interface Baseline {
  schema: 1;
  target: string;
  metrics: Measured;
}
export interface Verdict {
  name: string;
  value: number | null;
  status: "ok" | "over-budget" | "regression" | "unmeasured";
  detail: string;
}

export function median(xs: readonly number[]): number {
  const s = xs.toSorted((a, b) => a - b);
  const m = Math.floor(s.length / 2);
  if (s.length === 0) return Number.NaN;
  return s.length % 2 ? (s[m] as number) : ((s[m - 1] as number) + (s[m] as number)) / 2;
}

/** Judges one run against the absolute budgets and the baseline. Pure, so it is unit-testable. */
export function judge(defs: readonly MetricDef[], measured: Measured, baseline: Measured): Verdict[] {
  return defs.map((d) => {
    const value = measured[d.name] ?? null;
    if (value === null || Number.isNaN(value)) {
      return { name: d.name, value: null, status: "unmeasured", detail: "not measured in this environment" };
    }
    const over = d.dir === "max" ? value > d.budget : value < d.budget;
    if (over) {
      const op = d.dir === "max" ? "<=" : ">=";
      return { name: d.name, value, status: "over-budget", detail: `${value} breaks the ${op} ${d.budget} budget` };
    }
    const base = baseline[d.name];
    if (typeof base === "number" && d.dir === "max") {
      const limit = base + Math.max(base * d.rel, d.abs);
      if (value > limit) {
        return {
          name: d.name,
          value,
          status: "regression",
          detail: `${value} is over baseline ${base} plus slack (limit ${Math.round(limit)})`,
        };
      }
    }
    return { name: d.name, value, status: "ok", detail: `${value} within budget ${d.budget}` };
  });
}

// ---- bundle sizes ------------------------------------------------------------------------------

function measureBundles(): Measured {
  const dist = join(root, "plugins", "vscode", "dist");
  const out: Measured = { hostBundleBytes: null, uiInitialGzipBytes: null, uiLazyMaxGzipBytes: null };
  const host = join(dist, "extension.cjs");
  if (existsSync(host)) out.hostBundleBytes = statSync(host).size;
  const ui = join(dist, "ui");
  if (existsSync(ui)) {
    const gz = (f: string): number => gzipSync(readFileSync(join(ui, f))).length;
    const files = readdirSync(ui).filter((f) => f.endsWith(".js"));
    if (files.includes("index.js")) out.uiInitialGzipBytes = gz("index.js");
    const lazy = files.filter((f) => f !== "index.js").map(gz);
    out.uiLazyMaxGzipBytes = lazy.length ? Math.max(...lazy) : 0;
  }
  return out;
}

// ---- bridge RSS --------------------------------------------------------------------------------

function rssOf(pid: number): number | null {
  if (process.platform === "win32") {
    const r = spawnSync("tasklist", ["/FI", `PID eq ${pid}`, "/FO", "CSV", "/NH"], { encoding: "utf8" });
    const cols = r.stdout.trim().split('","');
    const mem = cols[4]?.replace(/[^0-9]/g, "");
    return mem ? Number(mem) * KIB : null;
  }
  const r = spawnSync("ps", ["-o", "rss=", "-p", String(pid)], { encoding: "utf8" });
  const kb = Number(r.stdout.trim());
  return Number.isFinite(kb) && kb > 0 ? kb * KIB : null;
}

function which(name: string): string | null {
  const probe = spawnSync(process.platform === "win32" ? "where" : "which", [name], { encoding: "utf8" });
  const first = probe.stdout.split(/\r?\n/)[0]?.trim();
  return probe.status === 0 && first ? first : null;
}

function findAsc(): string | null {
  const env = process.env.ASC_BIN;
  if (env && existsSync(env)) return env;
  return which("asc");
}

async function measureBridgeRss(): Promise<number | null> {
  const asc = findAsc();
  if (!asc) return null;
  const child = spawn(asc, ["api", "--stdio"], { stdio: ["pipe", "ignore", "ignore"], windowsHide: true });
  try {
    await new Promise((r) => setTimeout(r, 2000));
    if (child.exitCode !== null || child.pid === undefined) return null;
    return rssOf(child.pid);
  } finally {
    child.kill();
  }
}

// ---- extension-host probe ----------------------------------------------------------------------

interface ProbeResult {
  gc: boolean;
  activateMs: number | null;
  heapBytes: number;
  ascActive: boolean;
  error?: string;
}

function runWindow(code: string, folder: string): ProbeResult | null {
  const tmp = mkdtempSync(join(tmpdir(), "asc-probe-"));
  const out = join(tmp, "probe.json");
  const args = [
    "--disable-extensions",
    "--disable-workspace-trust",
    "--disable-gpu",
    "--new-window",
    "--user-data-dir",
    join(tmp, "user"),
    "--extensions-dir",
    join(tmp, "ext"),
    "--js-flags=--expose-gc",
    `--extensionDevelopmentPath=${join(here, "heap-probe")}`,
    `--extensionDevelopmentPath=${join(root, "plugins", "vscode")}`,
    folder,
  ];
  try {
    spawnSync(code, args, {
      env: { ...process.env, ASC_PROBE_OUT: out },
      timeout: 90_000,
      windowsHide: true,
      shell: process.platform === "win32" && /\.(cmd|bat)$/i.test(code),
    });
    return existsSync(out) ? (JSON.parse(readFileSync(out, "utf8")) as ProbeResult) : null;
  } finally {
    rmSync(tmp, { recursive: true, force: true });
  }
}

function fixture(kind: "asc" | "plain"): string {
  const dir = mkdtempSync(join(tmpdir(), `asc-fixture-${kind}-`));
  mkdirSync(join(dir, ".git"), { recursive: true });
  if (kind === "asc") writeFileSync(join(dir, ".git", "asc-workspace"), "");
  return dir;
}

function measureHost(runs: number): Measured {
  const none: Measured = { activateMs: null, nonAscCostMs: null, heapDeltaBytes: null };
  const envCode = process.env.ASC_VSCODE_PATH;
  const code = envCode && existsSync(envCode) ? envCode : which("code");
  if (!code) return none;
  const ascDir = fixture("asc");
  const plainDir = fixture("plain");
  try {
    const act: number[] = [];
    const inert: number[] = [];
    const heapAsc: number[] = [];
    const heapInert: number[] = [];
    for (let i = 0; i < runs; i++) {
      const a = runWindow(code, ascDir);
      const b = runWindow(code, plainDir);
      if (!a || !b || a.error || b.error || !a.gc || !a.ascActive) return none;
      if (a.activateMs !== null) act.push(a.activateMs);
      if (b.activateMs !== null) inert.push(b.activateMs);
      heapAsc.push(a.heapBytes);
      heapInert.push(b.heapBytes);
    }
    return {
      activateMs: act.length ? median(act) : null,
      nonAscCostMs: inert.length ? median(inert) : null,
      heapDeltaBytes: Math.max(0, median(heapAsc) - median(heapInert)),
    };
  } finally {
    rmSync(ascDir, { recursive: true, force: true });
    rmSync(plainDir, { recursive: true, force: true });
  }
}

// ---- graph -------------------------------------------------------------------------------------

function measureGraph(): number {
  const r = spawnSync("bunx", ["playwright", "test", "e2e/graph-perf.spec.ts"], {
    cwd: join(root, "packages", "asc-ui"),
    stdio: "inherit",
    shell: process.platform === "win32",
  });
  return r.status === 0 ? 1 : 0;
}

// ---- main --------------------------------------------------------------------------------------

export async function main(argv: readonly string[]): Promise<number> {
  const has = (flag: string): boolean => argv.includes(flag);
  const ti = argv.indexOf("--target");
  const target = ti >= 0 ? argv[ti + 1] : "";
  if (target !== "vscode") {
    process.stderr.write(
      target === "jetbrains"
        ? "jetbrains is P44b\n"
        : "usage: collect.ts --target vscode [--check] [--update] [--build] [--require-host] [--graph]\n",
    );
    return 2;
  }
  if (has("--build")) {
    const b = spawnSync("bun", ["scripts/bundle.ts"], {
      cwd: join(root, "plugins", "vscode"),
      stdio: "inherit",
      shell: process.platform === "win32",
    });
    if (b.status !== 0) return 1;
  }
  const measured: Measured = { ...measureBundles() };
  measured.bridgeRssBytes = await measureBridgeRss();
  Object.assign(measured, measureHost(5));
  measured.graphPerfPass = has("--graph") ? measureGraph() : null;

  const baselinePath = join(here, "baseline.vscode.json");
  const prior = existsSync(baselinePath) ? (JSON.parse(readFileSync(baselinePath, "utf8")) as Baseline).metrics : {};
  if (has("--update")) {
    // Metrics not measured here keep their previous baseline value.
    const merged: Measured = { ...prior };
    for (const [k, v] of Object.entries(measured)) if (v !== null) merged[k] = v;
    const base: Baseline = { schema: 1, target: "vscode", metrics: merged };
    writeFileSync(baselinePath, `${JSON.stringify(base, null, 2)}\n`);
    process.stdout.write(`wrote ${baselinePath}\n`);
  }
  const baseline = existsSync(baselinePath) ? (JSON.parse(readFileSync(baselinePath, "utf8")) as Baseline).metrics : {};
  const verdicts = judge(VSCODE_METRICS, measured, baseline);
  for (const v of verdicts) process.stdout.write(`${v.status.padEnd(11)} ${v.name.padEnd(20)} ${v.detail}\n`);
  if (!has("--check")) return 0;
  const hostMissing = verdicts.some((v) => v.status === "unmeasured" && (v.name === "activateMs" || v.name === "heapDeltaBytes"));
  if (hostMissing && has("--require-host")) {
    process.stderr.write("host metrics are required but VS Code or asc was not available\n");
    return 1;
  }
  return verdicts.some((v) => v.status === "over-budget" || v.status === "regression") ? 1 : 0;
}

if (import.meta.main) process.exit(await main(process.argv.slice(2)));
