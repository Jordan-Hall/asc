// Dry-run checker for the VS Code release workflow (SPEC 24.9). Everything but the token step:
//   release-check.ts                 workflow schema, package.json, `vsce ls`, token-less argument checks
//   release-check.ts --tag <ref>     tag vscode-v<semver> equals package.json version and has a CHANGELOG entry
//   release-check.ts --vsix <file>   the packaged VSIX holds the expected files and nothing else
// Exits 1 with one line per problem. No network and no tokens are used.
import { existsSync, readFileSync } from "node:fs";
import { dirname, resolve } from "node:path";
import { exit } from "node:process";
import { fileURLToPath } from "node:url";

const here = dirname(fileURLToPath(import.meta.url));
const pluginDir = resolve(here, "..");
const repoRoot = resolve(pluginDir, "../..");
export const WORKFLOW_PATH = resolve(repoRoot, ".forgejo/workflows/vscode-release.yml");

const SEMVER = /^\d+\.\d+\.\d+$/;
const TAG = /^vscode-v(\d+\.\d+\.\d+)$/;
const SHA_PIN = /@[0-9a-f]{40}(\s|$)/;
const TOKEN_ENV = ["VSCE_PAT", "OVSX_PAT"] as const;
/** Files the VSIX may hold; anything else is a packaging leak (see .vscodeignore). */
export const VSIX_ALLOWED = [
  /^package\.json$/,
  /^README\.md$/,
  /^CHANGELOG\.md$/,
  /^icon\.png$/,
  /^dist\/extension\.cjs$/,
  /^dist\/ui\//,
];
export const VSIX_REQUIRED = [
  "package.json",
  "README.md",
  "CHANGELOG.md",
  "icon.png",
  "dist/extension.cjs",
];

interface Step {
  name?: string;
  uses?: string;
  run?: string;
  env?: Record<string, string>;
  if?: string;
}
interface Workflow {
  on?: Record<string, unknown>;
  jobs?: Record<string, { steps?: Step[]; "runs-on"?: string }>;
}

export function checkTag(ref: string, version: string, changelog: string): string[] {
  const m = TAG.exec(ref);
  if (!m) return [`tag "${ref}" must look like vscode-v<MAJOR>.<MINOR>.<PATCH>`];
  const problems: string[] = [];
  if (m[1] !== version)
    problems.push(`tag version ${m[1]} differs from package.json version ${version}`);
  const heading = new RegExp(`^## \\[${version.replaceAll(".", "\\.")}\\]`, "m");
  if (!heading.test(changelog)) problems.push(`CHANGELOG.md has no "## [${version}]" entry`);
  return problems;
}

export function checkPackage(pkg: Record<string, unknown>): string[] {
  const problems: string[] = [];
  if (pkg.name !== "asc") problems.push('package.json: name must be "asc" (extension id asc.asc)');
  if (pkg.publisher !== "asc") problems.push('package.json: publisher must be "asc"');
  if (typeof pkg.version !== "string" || !SEMVER.test(pkg.version)) {
    problems.push("package.json: version must be MAJOR.MINOR.PATCH");
  }
  const engines = pkg.engines as { vscode?: string } | undefined;
  if (!engines?.vscode?.startsWith("^")) problems.push("package.json: engines.vscode is missing");
  if (pkg.enabledApiProposals !== undefined) {
    problems.push("package.json: enabledApiProposals must not be set (SPEC 24.4)");
  }
  return problems;
}

export function checkWorkflow(doc: unknown): string[] {
  const wf = doc as Workflow | null;
  if (!wf || typeof wf !== "object") return ["workflow is not a mapping"];
  const problems: string[] = [];
  const push = wf.on?.push as { tags?: string[] } | undefined;
  if (!push?.tags?.some((t) => t.startsWith("vscode-v")))
    problems.push("workflow: on.push.tags must match vscode-v*");
  if (!wf.on || !("workflow_dispatch" in wf.on))
    problems.push("workflow: workflow_dispatch (dry run) is missing");
  const jobs = Object.values(wf.jobs ?? {});
  if (jobs.length === 0) return [...problems, "workflow: no jobs"];
  for (const job of jobs) if (!job["runs-on"]) problems.push("workflow: a job has no runs-on");
  const steps = jobs.flatMap((j) => j.steps ?? []);
  for (const s of steps) {
    if (s.uses && !SHA_PIN.test(`${s.uses} `))
      problems.push(`workflow: "${s.uses}" is not pinned to a commit SHA`);
    const cmd = s.run ?? "";
    if (cmd.includes("${{") && /secrets\./.test(cmd)) {
      problems.push(`workflow: secret interpolated into a run line: ${cmd.trim()}`);
    }
    if (/\b(echo|printf)\b[^\n]*(VSCE_PAT|OVSX_PAT)/.test(cmd) || /\s(-p|--pat)(\s|=)/.test(cmd)) {
      problems.push(`workflow: token in argv or output: ${cmd.trim()}`);
    }
    if (/\bset -x\b/.test(cmd)) problems.push("workflow: set -x would trace tokens");
    for (const tok of TOKEN_ENV) {
      if (s.env && tok in s.env && s.env[tok] !== `\${{ secrets.${tok} }}`) {
        problems.push(`workflow: ${tok} must come from secrets.${tok}`);
      }
    }
  }
  const vsce = steps.find((s) => /\bvsce\s+publish\b/.test(s.run ?? ""));
  const ovsx = steps.find((s) => /\bovsx@[\d.]+\s+publish\b/.test(s.run ?? ""));
  if (!vsce) problems.push("workflow: no `vsce publish` step");
  else {
    if (!/--packagePath\s+\S+\.vsix/.test(vsce.run ?? "")) {
      problems.push("workflow: vsce publish must use --packagePath <file>.vsix");
    }
    if (vsce.env?.VSCE_PAT === undefined)
      problems.push("workflow: vsce publish needs env VSCE_PAT");
  }
  if (!ovsx) problems.push("workflow: no pinned `ovsx@<version> publish` step");
  else {
    if (!/publish\s+\S+\.vsix/.test(ovsx.run ?? ""))
      problems.push("workflow: ovsx publish must take a .vsix path");
    if (ovsx.env?.OVSX_PAT === undefined)
      problems.push("workflow: ovsx publish needs env OVSX_PAT");
  }
  for (const [i, s] of steps.entries()) {
    if (s !== vsce && s !== ovsx) continue;
    const before = steps
      .slice(0, i)
      .map((x) => x.run ?? "")
      .join("\n");
    if (!/release-check\.ts/.test(before))
      problems.push("workflow: release-check.ts must run before a publish step");
    if (!/bun run package|vsce package/.test(before)) {
      problems.push("workflow: the VSIX must be packaged before a publish step");
    }
  }
  return problems;
}

/** Checks a list of VSIX entries (from `vsce ls` or the zip directory). */
export function checkVsixFiles(files: string[]): string[] {
  const problems: string[] = [];
  const norm = files
    .map((f) =>
      f
        .trim()
        .replaceAll("\\", "/")
        .replace(/^extension\//, "")
        .replace(/^(readme|changelog)\.md$/i, (m) => m.toUpperCase().replace(/\.MD$/, ".md")),
    )
    .filter((f) => f !== "");
  for (const need of VSIX_REQUIRED)
    if (!norm.includes(need)) problems.push(`vsix: ${need} is missing`);
  for (const f of norm) {
    if (f === "[Content_Types].xml" || f === "extension.vsixmanifest") continue;
    if (!VSIX_ALLOWED.some((re) => re.test(f))) problems.push(`vsix: unexpected file ${f}`);
  }
  return problems;
}

/** The argument vectors for the publish tools: no token ever appears in argv. */
export function publishArgs(vsix: string): { vsce: string[]; ovsx: string[] } {
  return {
    vsce: ["vsce", "publish", "--packagePath", vsix, "--no-dependencies"],
    ovsx: ["ovsx", "publish", vsix],
  };
}

export function checkArgs(vsix: string, env: Record<string, string | undefined>): string[] {
  const problems: string[] = [];
  const { vsce, ovsx } = publishArgs(vsix);
  for (const argv of [vsce, ovsx]) {
    for (const a of argv) {
      if (/^(-p|--pat)(=|$)/.test(a)) problems.push(`args: ${argv[0]} passes a token flag`);
      for (const tok of TOKEN_ENV) {
        const value = env[tok];
        if (value && a.includes(value)) problems.push(`args: ${argv[0]} argv holds ${tok}`);
      }
    }
  }
  return problems;
}

/** Reads the entry names of a zip (a VSIX) from its central directory. */
export function zipEntries(buf: Uint8Array): string[] {
  const dv = new DataView(buf.buffer, buf.byteOffset, buf.byteLength);
  let eocd = -1;
  for (let i = buf.length - 22; i >= 0; i--) {
    if (dv.getUint32(i, true) === 0x06054b50) {
      eocd = i;
      break;
    }
  }
  if (eocd < 0) throw new Error("not a zip file");
  const count = dv.getUint16(eocd + 10, true);
  let p = dv.getUint32(eocd + 16, true);
  const dec = new TextDecoder();
  const names: string[] = [];
  for (let n = 0; n < count; n++) {
    if (dv.getUint32(p, true) !== 0x02014b50) throw new Error("corrupt zip directory");
    const nameLen = dv.getUint16(p + 28, true);
    const extraLen = dv.getUint16(p + 30, true);
    const commentLen = dv.getUint16(p + 32, true);
    names.push(dec.decode(buf.subarray(p + 46, p + 46 + nameLen)));
    p += 46 + nameLen + extraLen + commentLen;
  }
  return names;
}

async function spawn(
  cmd: string[],
  cwd: string,
): Promise<{ code: number; out: string; err: string }> {
  const p = Bun.spawn(cmd, {
    cwd,
    stdout: "pipe",
    stderr: "pipe",
    env: { ...process.env, VSCE_PAT: "", OVSX_PAT: "" },
  });
  const [out, err, code] = await Promise.all([
    new Response(p.stdout).text(),
    new Response(p.stderr).text(),
    p.exited,
  ]);
  return { code, out, err };
}

function vsceBin(): string[] {
  const local = resolve(
    pluginDir,
    "node_modules/.bin",
    process.platform === "win32" ? "vsce.exe" : "vsce",
  );
  return existsSync(local) ? [local] : ["bunx", "vsce"];
}

async function main(argv: string[]): Promise<string[]> {
  const pkg = JSON.parse(readFileSync(resolve(pluginDir, "package.json"), "utf8")) as Record<
    string,
    unknown
  >;
  const flag = (name: string) => {
    const i = argv.indexOf(name);
    return i >= 0 ? argv[i + 1] : undefined;
  };
  const tag = flag("--tag");
  if (tag !== undefined) {
    return checkTag(
      tag,
      String(pkg.version),
      readFileSync(resolve(pluginDir, "CHANGELOG.md"), "utf8"),
    );
  }
  const vsix = flag("--vsix");
  if (vsix !== undefined) {
    if (!existsSync(resolve(vsix))) return [`vsix: ${vsix} does not exist`];
    return checkVsixFiles(zipEntries(new Uint8Array(await Bun.file(resolve(vsix)).arrayBuffer())));
  }
  const problems = checkPackage(pkg);
  if (!existsSync(WORKFLOW_PATH)) problems.push(`workflow: ${WORKFLOW_PATH} is missing`);
  else problems.push(...checkWorkflow(Bun.YAML.parse(readFileSync(WORKFLOW_PATH, "utf8"))));
  const ls = await spawn([...vsceBin(), "ls", "--no-dependencies"], pluginDir);
  if (ls.code !== 0) problems.push(`vsce ls failed: ${ls.err.trim() || ls.out.trim()}`);
  else problems.push(...checkVsixFiles(ls.out.split("\n")));
  problems.push(...checkArgs("asc.vsix", process.env));
  return problems;
}

if (import.meta.main) {
  const problems = await main(process.argv.slice(2));
  if (problems.length > 0) {
    for (const p of problems) process.stderr.write(`release-check: ${p}\n`);
    exit(1);
  }
  process.stdout.write("release-check: ok\n");
}
