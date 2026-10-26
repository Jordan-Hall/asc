// VSIX smoke test and engine spike (SPEC 24.7.1, 24.9; PLAN P35b).
//   bun plugins/vscode/scripts/smoke-vsix.ts --editor vscodium|vscode|cursor [--vsix f] [--bin p]
//
// Installs the built VSIX into a throw-away editor profile, opens the `dirty` fixture and a non-asc
// folder, and asserts activation, SCM group counts (through the `asc.probe` command) and inertness.
// It also records the editor's engine version against `engines.vscode` and checks that every stable
// API the extension uses exists (the Cursor/VSCodium spike: `engines` is lowered only if one is missing).
//
// The editor comes from --bin / ASC_SMOKE_EDITOR_BIN (an executable or an install directory), then
// PATH, then a download (VS Code and VSCodium, Windows and Linux x64, https only) into the work dir.
// Secrets never reach argv or logs: the only environment passed on is a fixed allowlist.
import { spawn, spawnSync } from "node:child_process";
import { existsSync, mkdirSync, readFileSync, readdirSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { basename, dirname, join, resolve } from "node:path";
import { exit } from "node:process";
import {
  type EditorKind,
  type Report,
  exeNames,
  parseArgs,
  pathCandidates,
  satisfiesEngine,
  verdict,
} from "../test/smoke/lib";

const root = resolve(import.meta.dir, "..");
const log = (s: string): void => void process.stdout.write(`${s}\n`);
const die = (s: string, code = 1): never => {
  process.stderr.write(`smoke-vsix: ${s}\n`);
  return exit(code);
};

function run(
  cmd: string,
  args: string[],
  opts: { cwd?: string; env?: Record<string, string> } = {},
): string {
  const r = spawnSync(cmd, args, {
    cwd: opts.cwd,
    env: opts.env ?? safeEnv(),
    encoding: "utf8",
    windowsHide: true,
  });
  if (r.status !== 0)
    die(
      `${basename(cmd)} ${args[0] ?? ""} failed (${r.status}): ${(r.stderr || r.stdout).slice(0, 400)}`,
    );
  return r.stdout;
}

/** The environment passed to children: no tokens, no inherited secrets. */
function safeEnv(extra: Record<string, string> = {}): Record<string, string> {
  const keep = [
    "PATH",
    "Path",
    "SystemRoot",
    "SYSTEMROOT",
    "TEMP",
    "TMP",
    "TMPDIR",
    "HOME",
    "USERPROFILE",
    "APPDATA",
    "LOCALAPPDATA",
    "COMSPEC",
    "PATHEXT",
    "DISPLAY",
    "XDG_RUNTIME_DIR",
  ];
  const env: Record<string, string> = {};
  for (const k of keep) {
    const v = process.env[k];
    if (v !== undefined) env[k] = v;
  }
  return { ...env, ...extra };
}

interface Install {
  exe: string;
  cli: string;
  dir: string;
}

function asInstall(editor: EditorKind, p: string): Install | null {
  const names = exeNames(editor, process.platform);
  let dir = p;
  if (existsSync(p) && !readdirSafe(p)) dir = dirname(p);
  // A PATH shim lives in <install>/bin or <install>/resources/app/bin: walk up to the executable.
  for (let d = dir, i = 0; i < 5; i++, d = dirname(d)) {
    // Newer VS Code archives keep the app in a hash-named subfolder next to the executable.
    const bases = [d, ...(readdirSafe(d) ?? []).map((n) => join(d, n))];
    for (const n of names) {
      const exe = join(d, n);
      if (!existsSync(exe)) continue;
      for (const base of bases) {
        const cli = join(base, "resources", "app", "out", "cli.js");
        if (existsSync(cli)) return { exe, cli, dir: d };
      }
    }
  }
  return null;
}

function readdirSafe(p: string): string[] | null {
  try {
    return readdirSync(p);
  } catch {
    return null;
  }
}

async function download(editor: EditorKind, work: string): Promise<Install> {
  if (editor === "cursor")
    return die("Cursor has no stable download URL: pass --bin or put cursor on PATH", 2);
  const win = process.platform === "win32";
  if (!win && process.platform !== "linux") return die(`no download for ${process.platform}`, 2);
  if (process.arch !== "x64") return die(`no download for ${process.arch}`, 2);
  const dest = join(work, `editor-${editor}`);
  const hit = asInstall(editor, dest);
  if (hit) return hit;
  rmSync(dest, { recursive: true, force: true });
  mkdirSync(dest, { recursive: true });
  let url: string;
  if (editor === "vscode") {
    url = win
      ? "https://update.code.visualstudio.com/latest/win32-x64-archive/stable"
      : "https://update.code.visualstudio.com/latest/linux-x64/stable";
  } else {
    const res = await fetch("https://api.github.com/repos/VSCodium/vscodium/releases/latest", {
      headers: { accept: "application/vnd.github+json" },
    });
    if (!res.ok) return die(`VSCodium release lookup failed: ${res.status}`);
    const rel = (await res.json()) as { assets: { name: string; browser_download_url: string }[] };
    const want = win ? /^VSCodium-win32-x64-[\d.]+\.zip$/ : /^VSCodium-linux-x64-[\d.]+\.tar\.gz$/;
    const asset = rel.assets.find((a) => want.test(a.name));
    if (!asset) return die("no VSCodium archive in the latest release");
    url = asset.browser_download_url;
  }
  if (!url.startsWith("https://")) return die("refusing a non-https download");
  log(`downloading ${url}`);
  const res = await fetch(url, { redirect: "follow" });
  if (!res.ok) return die(`download failed: ${res.status}`);
  const archive = join(work, win ? `${editor}.zip` : `${editor}.tar.gz`);
  writeFileSync(archive, new Uint8Array(await res.arrayBuffer()));
  run(
    process.platform === "win32"
      ? join(process.env.SystemRoot ?? "C:\\Windows", "System32", "tar.exe")
      : "tar",
    ["-xf", archive, "-C", dest],
  );
  rmSync(archive, { force: true });
  const got = asInstall(editor, dest) ?? asInstall(editor, join(dest, readdirSync(dest)[0] ?? ""));
  return got ?? die("downloaded archive has no editor executable");
}

async function findEditor(
  editor: EditorKind,
  bin: string | undefined,
  work: string,
): Promise<Install> {
  if (bin) return asInstall(editor, resolve(bin)) ?? die(`${bin} is not a ${editor} install`, 2);
  const cmd = { vscodium: "codium", vscode: "code", cursor: "cursor" }[editor];
  for (const c of pathCandidates(
    cmd,
    process.env.PATH ?? process.env.Path ?? "",
    process.platform,
  )) {
    if (!existsSync(c)) continue;
    const i = asInstall(editor, c);
    if (i) return i;
  }
  return download(editor, work);
}

function findAsc(arg: string | undefined): string {
  if (arg) return resolve(arg);
  for (const c of pathCandidates(
    "asc",
    process.env.PATH ?? process.env.Path ?? "",
    process.platform,
  ))
    if (existsSync(c)) return c;
  return die("asc executable not found: pass --asc or set ASC_BIN", 2);
}

/** The `dirty` fixture: one staged, one modified, one untracked file (SPEC 24.7.2). */
function buildDirty(asc: string, dir: string): void {
  rmSync(dir, { recursive: true, force: true });
  mkdirSync(dir, { recursive: true });
  const env = safeEnv({ ASC_AUTHOR_NAME: "Smoke", ASC_AUTHOR_EMAIL: "smoke@example.invalid" });
  const c = (...a: string[]): string => run(asc, a, { cwd: dir, env });
  c("init", ".");
  writeFileSync(join(dir, "a.txt"), "one\n");
  c("add", "a.txt");
  c("commit", "-m", "init");
  writeFileSync(join(dir, "a.txt"), "two\n");
  writeFileSync(join(dir, "staged.txt"), "s\n");
  c("add", "staged.txt");
  writeFileSync(join(dir, "untracked.txt"), "u\n");
}

async function drive(
  i: Install,
  mode: "asc" | "plain",
  folder: string,
  work: string,
): Promise<Report | null> {
  const out = join(work, `report-${mode}.json`);
  rmSync(out, { force: true });
  const probe = join(root, "test", "smoke", "probe");
  const expect =
    mode === "asc" ? JSON.stringify({ conflicts: 0, staged: 1, changes: 1, untracked: 1 }) : "{}";
  const env = safeEnv({ ASC_SMOKE_MODE: mode, ASC_SMOKE_OUT: out, ASC_SMOKE_EXPECT: expect });
  const args = [
    folder,
    `--extensionDevelopmentPath=${probe}`,
    `--extensionTestsPath=${join(probe, "run.cjs")}`,
    `--user-data-dir=${join(work, "user")}`,
    `--extensions-dir=${join(work, "exts")}`,
    "--disable-workspace-trust",
    "--disable-gpu",
    "--no-sandbox",
    "--skip-welcome",
    "--skip-release-notes",
  ];
  const code = await new Promise<number>((res) => {
    const p = spawn(i.exe, args, { env, stdio: "ignore", windowsHide: true });
    const t = setTimeout(() => p.kill(), 120_000);
    p.on("exit", (c) => (clearTimeout(t), res(c ?? 1)));
    p.on("error", () => (clearTimeout(t), res(1)));
  });
  if (!existsSync(out)) {
    log(`[${mode}] editor exited ${code} without a report`);
    return null;
  }
  return JSON.parse(readFileSync(out, "utf8")) as Report;
}

async function main(): Promise<void> {
  const args = parseArgs(process.argv.slice(2), process.env);
  const pkg = JSON.parse(readFileSync(join(root, "package.json"), "utf8")) as {
    version: string;
    engines: { vscode: string };
  };
  const work = resolve(
    args.work ?? (existsSync("E:/tmp") ? "E:/tmp/asc-smoke" : join(tmpdir(), "asc-smoke")),
  );
  mkdirSync(work, { recursive: true });

  let vsix = args.vsix ? resolve(args.vsix) : join(root, `asc-${pkg.version}.vsix`);
  if (!args.vsix && !existsSync(vsix)) {
    log("building the VSIX");
    run("bun", ["run", "package"], { cwd: root });
  }
  if (!existsSync(vsix)) vsix = die(`${vsix} does not exist`);

  const asc = findAsc(args.asc);
  const install = await findEditor(args.editor, args.bin, work);
  const appPkg = JSON.parse(
    readFileSync(join(dirname(install.cli), "..", "package.json"), "utf8"),
  ) as { version: string };
  log(`editor ${args.editor} ${appPkg.version} (engines.vscode ${pkg.engines.vscode})`);

  rmSync(join(work, "user"), { recursive: true, force: true });
  rmSync(join(work, "exts"), { recursive: true, force: true });
  mkdirSync(join(work, "user", "User"), { recursive: true });
  writeFileSync(
    join(work, "user", "User", "settings.json"),
    JSON.stringify({
      "asc.path": asc,
      "update.mode": "none",
      "telemetry.telemetryLevel": "off",
      "security.workspace.trust.enabled": false,
      "extensions.autoUpdate": false,
    }),
  );
  run(
    install.exe,
    [
      install.cli,
      "--install-extension",
      vsix,
      `--extensions-dir=${join(work, "exts")}`,
      `--user-data-dir=${join(work, "user")}`,
      "--force",
    ],
    {
      env: safeEnv({ ELECTRON_RUN_AS_NODE: "1" }),
    },
  );

  const stamp = `${Date.now()}`;
  const dirty = join(work, `fixture-dirty-${stamp}`);
  const plain = join(work, `fixture-plain-${stamp}`);
  buildDirty(asc, dirty);
  rmSync(plain, { recursive: true, force: true });
  mkdirSync(plain, { recursive: true });
  writeFileSync(join(plain, "readme.txt"), "not an asc folder\n");

  const reports: Report[] = [];
  for (const [mode, folder] of [
    ["asc", dirty],
    ["plain", plain],
  ] as const) {
    const r = await drive(install, mode, folder, work);
    if (r) reports.push(r);
  }
  if (existsSync(join(plain, ".vscode", "settings.json"))) {
    reports.push({
      mode: "plain",
      editorVersion: appPkg.version,
      failures: ["workspace settings were written in a non-asc folder"],
      notes: [],
    });
  }
  for (const r of reports) for (const n of r.notes) log(`[${r.mode}] note: ${n}`);
  const problems = verdict(reports, args.strictProbe);
  // Engine spike: the host API version (not the product version; Cursor's differs) against `engines`.
  const hostVersion = reports[0]?.editorVersion ?? "";
  log(`engine spike: ${args.editor} host API ${hostVersion}, engines.vscode ${pkg.engines.vscode}`);
  if (hostVersion && !satisfiesEngine(hostVersion, pkg.engines.vscode)) {
    problems.push(
      `host API ${hostVersion} is below engines.vscode ${pkg.engines.vscode}: lower engines only if a stable API is missing`,
    );
  }
  if (!args.keep) {
    rmSync(dirty, { recursive: true, force: true });
    rmSync(plain, { recursive: true, force: true });
  }
  if (problems.length) {
    for (const p of problems) process.stderr.write(`FAIL ${p}\n`);
    exit(1);
  }
  log(`smoke ok on ${args.editor} ${appPkg.version}`);
}

if (import.meta.main) await main();
