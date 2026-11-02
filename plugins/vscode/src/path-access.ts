// Read-only paths from the repository's path rules (SPEC 15.5.1). `asc fetch` stores the levels
// the server gives this person in `<common dir>/asc-path-access`; a path under a rule directory
// below `write` is read-only. The worktree files already carry the read-only bit, this module
// lets the editor say why and refuse edits before a save is attempted.
import { readFile } from "node:fs/promises";
import { isAbsolute, join, resolve } from "node:path";

export const ACCESS_FILE = "asc-path-access";
const HEADER = "asc path-access 1";
const LEVELS = ["none", "build", "read", "write", "admin"] as const;
export type Level = (typeof LEVELS)[number];

export interface PathAccess {
  remote: string;
  repoLevel: Level;
  /** Rule directories (`""` = the root) with the level they give. */
  dirs: [string, Level][];
}

const rank = (l: Level) => LEVELS.indexOf(l);

/** Parses the stored file; null when it is not one this version knows. */
export function parseAccess(text: string): PathAccess | null {
  const lines = text.split("\n");
  if (lines.at(-1) === "") lines.pop();
  if (lines[0] !== HEADER) return null;
  const remote = lines[1]?.startsWith("remote ") ? lines[1].slice(7) : null;
  const repo = lines[2]?.startsWith("repo ") ? lines[2].slice(5) : null;
  if (remote === null || !isLevel(repo)) return null;
  const dirs: [string, Level][] = [];
  for (const line of lines.slice(3)) {
    const m = /^dir ([a-z]+)\t(.*)$/.exec(line);
    if (!m || !isLevel(m[1]) || !safeRel(m[2] ?? "")) return null;
    dirs.push([m[2] ?? "", m[1]]);
  }
  return { remote, repoLevel: repo, dirs };
}

function isLevel(s: string | null | undefined): s is Level {
  return (LEVELS as readonly string[]).includes(s ?? "");
}

/** The same check as the CLI: a plain `/`-separated path that never leaves the work tree. */
export function safeRel(p: string): boolean {
  if (p === "") return true;
  if (/[\\:\0\n\r]/.test(p)) return false;
  return p
    .split("/")
    .every(
      (c) =>
        c !== "" && !/[. ]$/.test(c) && !c.includes("~") && c.toLowerCase() !== ".git",
    );
}

function under(path: string, dir: string): boolean {
  return dir === "" || path === dir || path.startsWith(`${dir}/`);
}

/** Whether the path rules leave the repo-relative `path` writable. As in the CLI, the
 * repository level alone locks nothing: only a rule directory below `write` does. */
export function writable(a: PathAccess | null, path: string): boolean {
  if (!a) return true;
  let best: [string, Level] | null = null;
  for (const d of a.dirs) {
    if (under(path, d[0]) && (!best || d[0].length > best[0].length)) best = d;
  }
  return !best || rank(best[1]) >= rank("write");
}

/** The repository's common git directory for a worktree's git dir. */
export async function commonDir(gitDir: string): Promise<string> {
  try {
    const rel = (await readFile(join(gitDir, "commondir"), "utf8")).trim();
    return isAbsolute(rel) ? rel : resolve(gitDir, rel);
  } catch {
    return gitDir;
  }
}

/** The stored levels of the repository whose git dir is `gitDir`, or null. */
export async function loadAccess(gitDir: string): Promise<PathAccess | null> {
  try {
    return parseAccess(await readFile(join(await commonDir(gitDir), ACCESS_FILE), "utf8"));
  } catch {
    return null;
  }
}
