// JetBrains budget gate (SPEC 24.4.10, PLAN P44b). `collectJetbrains` and `checkJetbrains` are what P44a's
// collect.ts calls for `--target jetbrains`; this file also runs standalone:
// `bun bench/plugins/jetbrains.ts --check`.
//
// Measured here: jar size (the built plugin zip) and bridge RSS (a real `asc api --stdio` at idle).
// Read from a probe report: activation time and JVM heap delta. A test-only IDE run measures those (forced GC,
// used heap with the plugin enabled minus the same IDE with it disabled, median of 5) and writes JSON to
// ASC_JB_PROBE_REPORT or plugins/jetbrains/build/reports/asc-budget/probe.json.
// TODO(P44b-probe): the Gradle probe task belongs in plugins/jetbrains/build.gradle.kts, which another unit owns.
// TODO(P44a): wire `collectJetbrains` and `checkJetbrains` into bench/plugins/collect.ts when it lands.
import { spawn } from "node:child_process";
import { existsSync, readdirSync, readFileSync, statSync, writeFileSync } from "node:fs";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";

export type Metric = "activationMs" | "inertActivationMs" | "jvmHeapDeltaMiB" | "bridgeRssMiB" | "jarBytes";
export type Metrics = Record<Metric, number | null>;
export interface Baseline {
  schema: 1;
  target: "jetbrains";
  budgets: Record<Metric, number>;
  tolerance: { relative: number; absoluteMs: number; absoluteMiB: number; absoluteBytes: number };
  measured: Metrics;
}
export interface Finding {
  metric: Metric;
  kind: "budget" | "regression" | "missing";
  message: string;
}

export const METRICS: readonly Metric[] = ["activationMs", "inertActivationMs", "jvmHeapDeltaMiB", "bridgeRssMiB", "jarBytes"];
const here = dirname(fileURLToPath(import.meta.url));
const root = resolve(here, "../..");
const BASELINE_PATH = join(here, "baseline.jetbrains.json");

export function loadBaseline(path = BASELINE_PATH): Baseline {
  const b = JSON.parse(readFileSync(path, "utf8")) as Baseline;
  if (b.schema !== 1 || b.target !== "jetbrains") throw new Error(`unexpected baseline: ${path}`);
  return b;
}

/** Pure comparison: every metric must be present, within budget, and not regress past tolerance vs the baseline. */
export function checkJetbrains(m: Metrics, b: Baseline): Finding[] {
  const out: Finding[] = [];
  for (const k of METRICS) {
    const v = m[k];
    if (v === null || !Number.isFinite(v)) {
      out.push({ metric: k, kind: "missing", message: `${k}: not measured` });
      continue;
    }
    if (v > b.budgets[k]) out.push({ metric: k, kind: "budget", message: `${k}: ${v} exceeds budget ${b.budgets[k]}` });
    const base = b.measured[k];
    if (base !== null) {
      const slack = k.endsWith("Ms") ? b.tolerance.absoluteMs : k.endsWith("MiB") ? b.tolerance.absoluteMiB : b.tolerance.absoluteBytes;
      const limit = base * (1 + b.tolerance.relative) + slack;
      if (v > limit) out.push({ metric: k, kind: "regression", message: `${k}: ${v} regressed past ${limit.toFixed(1)} (baseline ${base})` });
    }
  }
  return out;
}

/** Size of the shipped plugin distribution zip (the jar budget of SPEC 24.4.10 applies to what ships). */
export function jarBytes(distDir = join(root, "plugins/jetbrains/build/distributions")): number | null {
  if (!existsSync(distDir)) return null;
  const zips = readdirSync(distDir).filter((f) => f.endsWith(".zip") && !f.includes("-signed"));
  if (zips.length === 0) return null;
  return Math.max(...zips.map((f) => statSync(join(distDir, f)).size));
}

export function readProbe(
  path = process.env.ASC_JB_PROBE_REPORT ?? join(root, "plugins/jetbrains/build/reports/asc-budget/probe.json"),
): Partial<Metrics> {
  if (!existsSync(path)) return {};
  const j = JSON.parse(readFileSync(path, "utf8")) as Record<string, unknown>;
  const pick = (k: Metric) => (typeof j[k] === "number" && Number.isFinite(j[k]) ? (j[k] as number) : null);
  return { activationMs: pick("activationMs"), inertActivationMs: pick("inertActivationMs"), jvmHeapDeltaMiB: pick("jvmHeapDeltaMiB") };
}

function rssMiB(pid: number): Promise<number | null> {
  return new Promise((res) => {
    const win = process.platform === "win32";
    const p = win
      ? spawn("tasklist", ["/FI", `PID eq ${pid}`, "/FO", "CSV", "/NH"], { stdio: ["ignore", "pipe", "ignore"] })
      : spawn("ps", ["-o", "rss=", "-p", String(pid)], { stdio: ["ignore", "pipe", "ignore"] });
    let s = "";
    p.stdout.on("data", (d) => (s += d));
    p.on("error", () => res(null));
    p.on("close", () => {
      // tasklist prints "Mem Usage" as e.g. "12,345 K"; ps prints KiB.
      const kib = win ? Number((s.trim().split('","').pop() ?? "").replace(/[^0-9]/g, "")) : Number(s.trim());
      res(Number.isFinite(kib) && kib > 0 ? kib / 1024 : null);
    });
  });
}

/** Idle RSS of one `asc api --stdio` bridge: spawn it, let it settle, sample three times, take the maximum. */
export async function bridgeRssMiB(asc = process.env.ASC_BIN ?? "asc", settleMs = 1500): Promise<number | null> {
  const child = spawn(asc, ["api", "--stdio"], { stdio: ["pipe", "ignore", "ignore"] });
  const failed = new Promise<null>((r) => child.on("error", () => r(null)));
  try {
    const run = (async () => {
      await new Promise((r) => setTimeout(r, settleMs));
      if (child.pid === undefined || child.exitCode !== null) return null;
      let max = 0;
      for (let i = 0; i < 3; i++) {
        const v = await rssMiB(child.pid);
        if (v === null) return null;
        max = Math.max(max, v);
        await new Promise((r) => setTimeout(r, 200));
      }
      return max;
    })();
    return await Promise.race([run, failed]);
  } finally {
    child.stdin?.end();
    child.kill();
  }
}

export async function collectJetbrains(): Promise<Metrics> {
  const probe = readProbe();
  return {
    activationMs: probe.activationMs ?? null,
    inertActivationMs: probe.inertActivationMs ?? null,
    jvmHeapDeltaMiB: probe.jvmHeapDeltaMiB ?? null,
    bridgeRssMiB: await bridgeRssMiB(),
    jarBytes: jarBytes(),
  };
}

async function main(argv: string[]): Promise<number> {
  const known = new Set(["--check", "--update-baseline", "--target", "jetbrains"]);
  for (const a of argv) if (!known.has(a)) throw new Error(`unknown argument: ${a}`);
  const baseline = loadBaseline();
  const m = await collectJetbrains();
  process.stdout.write(`${JSON.stringify(m, null, 2)}
`);
  if (argv.includes("--update-baseline")) {
    const missing = METRICS.filter((k) => m[k] === null);
    if (missing.length) throw new Error(`cannot record a baseline, not measured: ${missing.join(", ")}`);
    writeFileSync(BASELINE_PATH, `${JSON.stringify({ ...baseline, measured: m }, null, 2)}\n`);
    return 0;
  }
  if (!argv.includes("--check")) return 0;
  const findings = checkJetbrains(m, baseline);
  for (const f of findings) process.stderr.write(`FAIL ${f.kind}: ${f.message}
`);
  return findings.length === 0 ? 0 : 1;
}

if (import.meta.main) {
  main(process.argv.slice(2)).then(
    (c) => process.exit(c),
    (e) => {
      process.stderr.write(`${e instanceof Error ? e.message : String(e)}
`);
      process.exit(2);
    },
  );
}
