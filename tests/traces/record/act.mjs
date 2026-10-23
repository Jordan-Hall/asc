// Runs a non-git action and marks it in the git trace so the converter can place it.
//   node act.mjs '<sh snippet>' [args...]
//   node act.mjs --file <script> [args...]     (the snippet is the script text)
// The snippet runs with `sh -c`, args as "$@", in the current directory. It is also stored under
// $ASC_TRACE_ACTS so that the trace carries the snippet text. Used by scenarios (the `act` helper)
// and by tool tasks (a lint-staged task that rewrites a file).
import { spawnSync } from "node:child_process";
import { mkdirSync, readFileSync, writeFileSync } from "node:fs";
import { join } from "node:path";

const acts = process.env.ASC_TRACE_ACTS;
const sh = process.env.ASC_TRACE_SH || "sh";
if (!acts) {
  process.stderr.write("act.mjs: ASC_TRACE_ACTS is not set (run through record.sh)\n");
  process.exit(2);
}
const argv = process.argv.slice(2);
const fromFile = argv[0] === "--file";
const snippet = fromFile ? readFileSync(argv[1], "utf8").replaceAll("\r\n", "\n").trimEnd() : argv[0];
const args = argv.slice(fromFile ? 2 : 1);
mkdirSync(acts, { recursive: true });
const counter = join(acts, "count");
let n = 1;
try {
  n = Number(readFileSync(counter, "utf8")) + 1;
} catch {}
writeFileSync(counter, String(n));
const run = spawnSync(sh, ["-c", snippet, "act", ...args], { stdio: "inherit" });
writeFileSync(join(acts, `${n}.json`), JSON.stringify({ snippet, args }));
spawnSync("git", ["-c", `asc.mark=act:${n}`, "version"], { stdio: "ignore" });
process.exit(run.status ?? 1);
