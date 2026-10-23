// Converts a GIT_TRACE2_EVENT log of one recorded scenario into a .trace file (TOML).
//   node to-trace.mjs --trace2 <jsonl> --acts <dir> --root <fixture root> --scenario <file> --out <trace>
//
// What is kept: every git invocation that a tool made (argv, the GIT_* environment git reports,
// the exit code), the driver commands, the non-git actions (marked by act.mjs) and the hooks git
// ran, with the tool calls made inside each hook. What is dropped: git's own helper processes
// (maintenance, fsmonitor, credential helpers), which are children of a git process but not of a
// hook, and the version and ancestry noise.
//
// Not recorded: the working directory (every call ran at the fixture root or its hook's root) and
// stdin (no tool in the corpus pipes data into git; a case that does must say so in its README).
import { readFileSync, writeFileSync } from "node:fs";
import { join } from "node:path";

const arg = (name) => {
  const i = process.argv.indexOf(`--${name}`);
  if (i < 0 || i + 1 >= process.argv.length) throw new Error(`missing --${name}`);
  return process.argv[i + 1];
};
const root = arg("root").replaceAll("\\", "/");
const acts = arg("acts");
const scenarioPath = arg("scenario").replaceAll("\\", "/");
const scenario = readFileSync(scenarioPath, "utf8").replaceAll("\r\n", "\n");

// Scenario metadata: `#@ key: value` lines (a repeated key joins its values with a space).
const meta = {};
for (const line of scenario.split("\n")) {
  const m = /^#@ (\w+):\s*(.*)$/.exec(line);
  if (m) meta[m[1]] = meta[m[1]] ? `${meta[m[1]]} ${m[2]}` : m[2];
}
for (const key of ["tool", "version", "provenance", "driver"]) {
  if (!meta[key]) throw new Error(`scenario lacks "#@ ${key}:"`);
}

// ---- parse -------------------------------------------------------------------------------------
const procs = new Map(); // sid -> process
const order = [];
let seq = 0;
let gitVersion = "";
for (const line of readFileSync(arg("trace2"), "utf8").split("\n")) {
  if (!line.trim()) continue;
  const e = JSON.parse(line);
  seq += 1;
  if (e.event === "version" && !gitVersion) gitVersion = /^\d+\.\d+\.\d+/.exec(e.exe)?.[0] ?? e.exe;
  let p = procs.get(e.sid);
  if (!p) {
    p = { sid: e.sid, argv: null, start: seq, env: {}, code: null, windows: [], open: new Map() };
    procs.set(e.sid, p);
    order.push(p);
  }
  if (e.event === "start") {
    p.argv = e.argv.slice(1);
    p.start = seq;
  } else if (e.event === "def_param" && e.scope === "command") {
    p.env[e.param] = e.value;
  } else if (e.event === "exit") {
    p.code = e.code;
  } else if (e.event === "child_start" && e.child_class === "hook") {
    p.open.set(e.child_id, { name: e.hook_name, path: e.argv[0], from: seq, to: Infinity });
  } else if (e.event === "child_exit" && p.open.has(e.child_id)) {
    const w = p.open.get(e.child_id);
    w.to = seq;
    w.code = e.code;
    p.windows.push(w);
    p.open.delete(e.child_id);
  }
}
for (const p of procs.values()) for (const w of p.open.values()) p.windows.push(w);

const parentOf = (p) => {
  const at = p.sid.lastIndexOf("/");
  return at < 0 ? null : (procs.get(p.sid.slice(0, at)) ?? null);
};
const markOf = (p) => {
  const at = p.argv?.indexOf("-c") ?? -1;
  const v = at >= 0 ? p.argv[at + 1] : "";
  return v.startsWith("asc.mark=") ? v.slice(8) : null;
};

// ---- normalise ---------------------------------------------------------------------------------
const esc = (s) => s.replace(/[.*+?^${}()|[\]\\]/g, "\\$&");
const msys = root.replace(/^([A-Za-z]):/, (_, d) => `/${d.toLowerCase()}`);
// The fixture root becomes ${ROOT}; the path after it (a Windows spelling) gets forward slashes.
// Text elsewhere keeps its backslashes: a shell snippet is not a path.
const rootRe = new RegExp(String.raw`(?:${[root, msys, root.replaceAll("/", "\\")].map(esc).join("|")})([\\/][^\s"']*)?`, "gi");
// ${GITDIR} is the git directory the replaying side reports (`rev-parse --absolute-git-dir`):
// an asc workspace keeps its git directory elsewhere, so `${ROOT}/.git` would name nothing there.
const norm = (s) =>
  s
    .replace(rootRe, (_, rest) => "${ROOT}" + (rest ?? "").replaceAll("\\", "/"))
    .replace(/^\.git(?=\/|$)/, "${GITDIR}")
    .replace(/\$\{ROOT\}\/\.git(?=\/|$)/g, "${GITDIR}");
const baseline = { GIT_EDITOR: "true" };
const callEnv = (p) => {
  const env = {};
  for (const [k, v] of Object.entries(p.env)) {
    if (k === "GIT_EXEC_PATH" || baseline[k] === v) continue;
    env[k] = norm(v);
  }
  return env;
};

// ---- walk --------------------------------------------------------------------------------------
// A double-quoted shell word that keeps ${ROOT} live (the replay defines ROOT).
const dq = (a) => {
  const parts = norm(a).split("${ROOT}");
  return `"${parts.map((p) => p.replace(/[\\"$`]/g, "\\$&")).join("${ROOT}")}"`;
};
const actText = ({ snippet, args }) => {
  const body = norm(snippet);
  return args.length ? `set -- ${args.map(dq).join(" ")}; ${body}` : body;
};
// The shim hands repository-independent invocations to real git (SPEC §11.4); they are outside
// the native table by design. Mirrors `independent` in crates/asc/src/shim.rs for the commands a
// tool can send: `version <args>`, `help` and `credential*`.
const routesToRealGit = (argv) => {
  const rest = [];
  for (let i = 0; i < argv.length; i += 1) {
    if (argv[i] === "-c" || argv[i] === "-C") i += 1;
    else if (!argv[i].startsWith("-") || rest.length) rest.push(argv[i]);
  }
  const [word = "", ...args] = rest;
  if (word === "version") return args.length > 0;
  return word === "help" || word.startsWith("credential");
};
const callOf = (p, from) => {
  const m = markOf(p);
  if (m?.startsWith("act:")) {
    const a = JSON.parse(readFileSync(join(acts, `${m.slice(4)}.json`), "utf8"));
    return { sh: actText(a), from };
  }
  const git = p.argv.map(norm);
  return { git, env: callEnv(p), exit: p.code ?? 0, from, real: routesToRealGit(git) };
};
// git's --output does not create the directory; the tool does, which no git trace shows. A
// `mkdir` step before the call stands in for it.
const withOutputDir = (c) => {
  const at = c.git?.indexOf("--output") ?? -1;
  const target = at >= 0 ? c.git[at + 1] : undefined;
  if (!target?.startsWith("${ROOT}") && !target?.startsWith("${GITDIR}")) return [c];
  return [{ sh: `mkdir -p "$(dirname "${target}")"`, from: c.from }, c];
};
const hookCalls = (p, from) => {
  const out = [];
  for (const w of p.windows.toSorted((a, b) => a.from - b.from)) {
    const calls = order
      .filter((c) => c.argv && parentOf(c) === p && c.start > w.from && c.start < w.to)
      .flatMap((c) => withOutputDir(callOf(c, from)));
    if (calls.length) out.push({ name: w.name, path: norm(w.path), exit: w.code ?? 0, calls });
  }
  return out;
};
const steps = [];
let region = null; // the tool whose directly-run git calls are being collected
for (const p of order) {
  if (!p.argv || parentOf(p)) continue; // top-level processes only; hook children attach below
  const m = markOf(p);
  if (m?.startsWith("begin:")) {
    region = m.slice(6);
    continue;
  }
  if (m?.startsWith("end:")) {
    region = null;
    continue;
  }
  const prepared = withOutputDir(callOf(p, region ?? "driver"));
  const step = prepared.at(-1); // the call itself; a mkdir step may precede it
  if (step.git) {
    const hooks = hookCalls(p, meta.tool);
    if (hooks.length) step.hooks = hooks;
  }
  steps.push(...prepared);
}

// ---- emit --------------------------------------------------------------------------------------
const s = (v) => JSON.stringify(v);
const lit = (v) => (v.includes("\n") ? `'''\n${v}\n'''` : s(v));
const emitCall = (prefix, c) => {
  let t = `[[${prefix}]]\n`;
  if (c.sh !== undefined) {
    t += `sh = ${lit(c.sh)}\nfrom = ${s(c.from)}\n`;
  } else {
    t += `git = [${c.git.map(s).join(", ")}]\nfrom = ${s(c.from)}\n`;
    if (c.real) t += 'routes = "real"\n';
    const env = Object.entries(c.env).map(([k, v]) => `${k} = ${s(v)}`);
    if (env.length) t += `env = { ${env.join(", ")} }\n`;
    t += `exit = ${c.exit}\n`;
  }
  return `${t}\n`;
};
let body = "";
for (const step of steps) {
  body += emitCall("step", step);
  for (const h of step.hooks ?? []) {
    body += `[[step.hook]]\nname = ${s(h.name)}\npath = ${s(h.path)}\nexit = ${h.exit}\n\n`;
    for (const c of h.calls) body += emitCall("step.hook.call", c);
  }
}
const head = `# Recorded by tests/traces/record/record.sh from record/scenarios/${scenarioPath.split("/record/scenarios/")[1]}
tool = ${s(meta.tool)}
version = ${s(meta.version)}
provenance = ${s(meta.provenance)}
git = ${s(gitVersion)}
driver = ${s(meta.driver)}

`;
writeFileSync(arg("out"), `${head}${body.trimEnd()}\n`);
