// asc-workspace detection (SPEC 24.4.1): one small read of `<folder>/.git` plus one `stat` of
// `<gitdir>/asc-workspace`. No glob, no directory walk, no bridge. Per-root state (Status snapshot,
// Watch, event coalescing) arrives with P25.
import { readFile, stat } from "node:fs/promises";
import { isAbsolute, join, resolve } from "node:path";

export interface DetectFs {
  /** At most `max` bytes of the file; rejects with ENOENT or EISDIR. */
  readSmall(path: string, max: number): Promise<string>;
  isFile(path: string): Promise<boolean>;
}

const MAX_GIT_FILE = 4096;

export const nodeDetectFs: DetectFs = {
  async readSmall(path, max) {
    const buf = await readFile(path);
    return buf.subarray(0, max).toString("utf8");
  },
  async isFile(path) {
    try {
      return (await stat(path)).isFile();
    } catch {
      return false;
    }
  },
};

/** The git directory a worktree's `.git` points to, or null when there is none. */
export async function resolveGitDir(folder: string, fs: DetectFs): Promise<string | null> {
  const dotGit = join(folder, ".git");
  let text: string;
  try {
    text = await fs.readSmall(dotGit, MAX_GIT_FILE);
  } catch (e) {
    // A `.git` directory cannot be read as a file: that is the common layout.
    const code = (e as NodeJS.ErrnoException).code;
    return code === "EISDIR" ? dotGit : null;
  }
  const m = /^gitdir:\s*(.+?)\s*$/m.exec(text);
  if (!m?.[1]) return null;
  return isAbsolute(m[1]) ? m[1] : resolve(folder, m[1]);
}

/** True when the folder is an asc workspace: the git dir holds an `asc-workspace` marker. */
export async function isAscWorkspace(
  folder: string,
  fs: DetectFs = nodeDetectFs,
): Promise<boolean> {
  const gitDir = await resolveGitDir(folder, fs);
  return gitDir !== null && (await fs.isFile(join(gitDir, "asc-workspace")));
}

/** The folders (in order) that are asc workspaces. Checks run concurrently. */
export async function detectAscFolders(
  folders: readonly string[],
  fs: DetectFs = nodeDetectFs,
): Promise<string[]> {
  const hits = await Promise.all(folders.map((f) => isAscWorkspace(f, fs)));
  return folders.filter((_, i) => hits[i]);
}
