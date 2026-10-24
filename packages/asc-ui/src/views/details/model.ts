// Commit details model (SPEC §24.3.3): message, files, change, PR, versions. Pure functions;
// the DOM lives in view.ts and the wire mapping in from-proto.ts.

export type FileStatus = "added" | "modified" | "deleted" | "renamed" | "copied" | "typechange" | "unknown";

export interface DetailFile {
  readonly path: string;
  readonly oldPath?: string;
  readonly status: FileStatus;
  readonly added?: number;
  readonly removed?: number;
  readonly binary?: boolean;
}

export interface DetailPerson {
  readonly name: string;
  readonly email: string;
  /** Unix seconds. */
  readonly time: number;
}

export interface DetailReview {
  readonly host: string;
  readonly number: number;
  readonly url: string;
  readonly state: string;
  readonly draft: boolean;
}

export interface DetailVersion {
  readonly v: number;
  readonly rev: string;
  /** Unix seconds. */
  readonly at: number;
  readonly actor: string;
}

export interface DetailsData {
  readonly id: string;
  readonly parents: readonly string[];
  readonly message: string;
  readonly author: DetailPerson;
  readonly committer?: DetailPerson;
  /** `%G?` signature status from `Log`; "G" is a good signature. Absent when not reported. */
  readonly signature?: string;
  readonly change?: string;
  readonly review?: DetailReview;
  readonly versions: readonly DetailVersion[];
  readonly files: readonly DetailFile[];
  readonly truncated?: boolean;
}

export interface MessageParts {
  readonly subject: string;
  readonly body: string;
}

/** Subject is the first line; the body is the rest with surrounding blank lines trimmed. */
export function splitMessage(message: string): MessageParts {
  const text = message.replace(/\r\n/g, "\n");
  const nl = text.indexOf("\n");
  if (nl < 0) return { subject: text.trim(), body: "" };
  return { subject: text.slice(0, nl).trim(), body: text.slice(nl + 1).replace(/^\n+|\s+$/g, "") };
}

const STATUS: Record<string, FileStatus> = {
  A: "added",
  M: "modified",
  D: "deleted",
  R: "renamed",
  C: "copied",
  T: "typechange",
};

/** Maps a git status letter (`R100` keeps its first letter) to a status. */
export function statusOf(raw: string): FileStatus {
  return STATUS[raw.slice(0, 1).toUpperCase()] ?? "unknown";
}

export const STATUS_LETTER: Record<FileStatus, string> = {
  added: "A",
  modified: "M",
  deleted: "D",
  renamed: "R",
  copied: "C",
  typechange: "T",
  unknown: "?",
};

/** Files sorted by path so the list reads like a tree. Stable and pure. */
export function sortFiles<T extends { readonly path: string }>(files: readonly T[]): T[] {
  return files.toSorted((a, b) => (a.path < b.path ? -1 : a.path > b.path ? 1 : 0));
}

export interface Totals {
  readonly files: number;
  readonly added: number;
  readonly removed: number;
  /** False when no file carries counts, so the view omits the totals rather than show zero. */
  readonly known: boolean;
}

export function totals(files: readonly DetailFile[]): Totals {
  let added = 0;
  let removed = 0;
  let known = false;
  for (const f of files) {
    if (f.added !== undefined || f.removed !== undefined) known = true;
    added += f.added ?? 0;
    removed += f.removed ?? 0;
  }
  return { files: files.length, added, removed, known };
}

/**
 * Multi-diff targets for "open all changes" (`vscode.changes` or the native diff): one per text
 * file against the commit's first parent. A root commit has no parent, so the left side is empty
 * and the host shows an addition. Renamed files keep the old path on the left.
 */
export interface DiffPair {
  readonly path: string;
  readonly oldPath: string;
  readonly leftRev: string;
  readonly rightRev: string;
}

export function diffPairs(data: Pick<DetailsData, "id" | "parents" | "files">): DiffPair[] {
  const left = data.parents[0] ?? "";
  return sortFiles(data.files)
    .filter((f) => f.binary !== true)
    .map((f) => ({ path: f.path, oldPath: f.oldPath ?? f.path, leftRev: left, rightRev: data.id }));
}

export function isSigned(sig: string | undefined): boolean {
  return sig === "G" || sig === "U";
}

/** Only https URLs from the server are opened as links. */
export function safeUrl(url: string): string | undefined {
  try {
    const u = new URL(url);
    return u.protocol === "https:" ? u.href : undefined;
  } catch {
    return undefined;
  }
}

export function shortId(id: string): string {
  return id.slice(0, 12);
}
