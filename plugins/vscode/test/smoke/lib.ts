// Pure helpers for scripts/smoke-vsix.ts (kept apart so `bun test` can cover them).
import { delimiter, join } from "node:path";

export type EditorKind = "vscodium" | "vscode" | "cursor";
export interface Args {
  editor: EditorKind;
  vsix: string | undefined;
  bin: string | undefined;
  asc: string | undefined;
  work: string | undefined;
  strictProbe: boolean;
  keep: boolean;
}

const EDITORS: readonly string[] = ["vscodium", "vscode", "cursor"];

export function parseArgs(argv: readonly string[], env: Record<string, string | undefined>): Args {
  const a: Args = {
    editor: "vscodium",
    vsix: undefined,
    bin: env.ASC_SMOKE_EDITOR_BIN,
    asc: env.ASC_BIN,
    work: env.ASC_SMOKE_WORK,
    strictProbe: false,
    keep: false,
  };
  for (let i = 0; i < argv.length; i++) {
    const k = argv[i];
    const val = (): string => {
      const v = argv[++i];
      if (v === undefined || v.startsWith("--")) throw new Error(`${k} needs a value`);
      return v;
    };
    if (k === "--editor") {
      const v = val();
      if (!EDITORS.includes(v)) throw new Error(`--editor must be one of ${EDITORS.join(", ")}`);
      a.editor = v as EditorKind;
    } else if (k === "--vsix") a.vsix = val();
    else if (k === "--bin") a.bin = val();
    else if (k === "--asc") a.asc = val();
    else if (k === "--work") a.work = val();
    else if (k === "--strict-probe") a.strictProbe = true;
    else if (k === "--keep") a.keep = true;
    else throw new Error(`unknown argument ${k}`);
  }
  return a;
}

/** Executable base names to look for inside an install directory. */
export function exeNames(editor: EditorKind, platform: NodeJS.Platform): string[] {
  const win = platform === "win32";
  if (editor === "vscodium") return win ? ["VSCodium.exe"] : ["codium", "vscodium"];
  if (editor === "cursor") return win ? ["Cursor.exe"] : ["cursor"];
  return win ? ["Code.exe"] : ["code"];
}

/** Minimum major.minor from an `engines.vscode` range, or null. */
export function engineMin(range: string): [number, number] | null {
  const m = /(\d+)\.(\d+)/.exec(range);
  return m ? [Number(m[1]), Number(m[2])] : null;
}

/** True when `version` (x.y.z) is at least the engine minimum. */
export function satisfiesEngine(version: string, range: string): boolean {
  const min = engineMin(range);
  const v = /^(\d+)\.(\d+)/.exec(version);
  if (!min || !v) return false;
  const [maj, mnr] = [Number(v[1]), Number(v[2])];
  return maj > min[0] || (maj === min[0] && mnr >= min[1]);
}

export interface Report {
  mode: string;
  editorVersion: string;
  failures: string[];
  notes: string[];
  probeMissing?: boolean;
  missingApis?: string[];
}

/** Reasons the run fails given the reports; a missing probe fails only in strict mode. */
export function verdict(reports: readonly Report[], strictProbe: boolean): string[] {
  const out: string[] = [];
  for (const r of reports) {
    for (const f of r.failures) out.push(`[${r.mode}] ${f}`);
    if (strictProbe && r.probeMissing) out.push(`[${r.mode}] asc.probe is not registered`);
  }
  if (reports.length !== 2) out.push(`expected 2 reports, got ${reports.length}`);
  return out;
}

/** Candidate paths for a command name on PATH. */
export function pathCandidates(name: string, path: string, platform: NodeJS.Platform): string[] {
  const exts = platform === "win32" ? [".cmd", ".exe", ""] : [""];
  return path
    .split(delimiter)
    .filter(Boolean)
    .flatMap((d) => exts.map((e) => join(d, name + e)));
}
