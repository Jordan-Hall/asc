// Repo service reads. The only module (with changes.ts) that knows service and method names.
import {
  type BridgeTransport,
  callStream,
  callUnary,
  type CallParams,
  toHex,
} from "@asc/asc-client";
import { Repo } from "@asc/asc-client/repo";
import type {
  CommitView,
  EntryKind,
  FileView,
  RefsView,
  RefView,
  TreeRow,
  TreeView,
} from "./types";

/** Files larger than this are cut off in the code view; the raw download is the way to read more. */
export const MAX_VIEW_BYTES = 4 * 1024 * 1024;

const text = new TextDecoder();

export function shortId(hex: string): string {
  return hex.slice(0, 10);
}

export function entryKind(mode: number): EntryKind {
  const type = mode & 0o170000;
  if (type === 0o040000) return "dir";
  if (type === 0o120000) return "symlink";
  if (type === 0o160000) return "submodule";
  return mode & 0o111 ? "exec" : "file";
}

function refKind(name: string): RefView["kind"] {
  if (name.startsWith("refs/heads/")) return "branch";
  if (name.startsWith("refs/tags/")) return "tag";
  return "other";
}

function shortRef(name: string): string {
  return name.replace(/^refs\/(heads|tags)\//, "");
}

export async function listRefs(
  t: BridgeTransport,
  repo: string,
  p?: CallParams,
): Promise<RefsView> {
  const res = await callUnary(
    t,
    Repo.method.listRefs,
    { repo: { name: repo }, prefix: "refs/", limit: 500 },
    p,
  );
  const refs: RefView[] = res.refs.map((r) => {
    const target = r.target?.t;
    const id = target?.case ? toHex(target.value) : "";
    return { name: r.name, short: shortRef(r.name), kind: refKind(r.name), id };
  });
  const defaultBranch =
    shortRef(res.head) || refs.find((r) => r.kind === "branch")?.short || "main";
  return { defaultBranch, refs };
}

export async function resolve(
  t: BridgeTransport,
  repo: string,
  rev: string,
  p?: CallParams,
): Promise<string | undefined> {
  const res = await callUnary(t, Repo.method.resolve, { repo: { name: repo }, revspecs: [rev] }, p);
  const id = res.ids[0];
  return id && id.length > 0 ? toHex(id) : undefined;
}

function sortRows(rows: TreeRow[]): TreeRow[] {
  return rows.toSorted((a, b) => {
    const da = a.kind === "dir" ? 0 : 1;
    const db = b.kind === "dir" ? 0 : 1;
    return da - db || a.name.localeCompare(b.name, "en", { numeric: true });
  });
}

/** One directory level. Streams: `emit` receives the rows so far. */
export async function readTree(
  t: BridgeTransport,
  repo: string,
  rev: string,
  path: string,
  emit: (v: TreeView) => void,
  p?: CallParams,
): Promise<TreeView> {
  const rows: TreeRow[] = [];
  const enc = new TextEncoder();
  const stream = callStream(
    t,
    Repo.method.readTree,
    { repo: { name: repo }, rev, path: enc.encode(path), recursive: false, limit: 0 },
    p,
  );
  for await (const batch of stream) {
    for (const e of batch.entries) {
      rows.push({
        name: text.decode(e.name),
        path: text.decode(e.path),
        kind: entryKind(e.mode),
        size: Number(e.size),
      });
    }
    emit({ rows: sortRows([...rows]), done: false });
  }
  return { rows: sortRows(rows), done: true };
}

export async function lastCommit(
  t: BridgeTransport,
  repo: string,
  rev: string,
  path: string,
  p?: CallParams,
): Promise<CommitView | undefined> {
  const stream = callStream(
    t,
    Repo.method.log,
    { repo: { name: repo }, include: [rev], paths: path ? [path] : [], limit: 1 },
    p,
  );
  for await (const batch of stream) {
    const e = batch.entries[0];
    if (!e) continue;
    const id = toHex(e.id);
    return {
      id,
      short: shortId(id),
      subject: text.decode(e.message).split("\n", 1)[0] ?? "",
      author: text.decode(e.author?.name ?? new Uint8Array()),
      time: Number(e.author?.time ?? 0n),
    };
  }
  return undefined;
}

/** Streams a file as text lines. Stops at `MAX_VIEW_BYTES` and flags binary content. */
export async function readFile(
  t: BridgeTransport,
  repo: string,
  rev: string,
  path: string,
  emit: (v: FileView) => void,
  p?: CallParams,
): Promise<FileView> {
  const decoder = new TextDecoder("utf-8", { fatal: false });
  const view: FileView = {
    lines: [],
    count: 0,
    size: 0,
    binary: false,
    truncated: false,
    done: false,
  };
  let partial = "";
  let read = 0;
  const stream = callStream(
    t,
    Repo.method.readFile,
    { repo: { name: repo }, at: { case: "revPath", value: `${rev}:${path}` } },
    p,
  );
  for await (const chunk of stream) {
    view.size = Number(chunk.totalSize);
    if (read === 0 && chunk.data.includes(0)) {
      view.binary = true;
      break;
    }
    read += chunk.data.length;
    const pieces = (partial + decoder.decode(chunk.data, { stream: true })).split("\n");
    partial = pieces.pop() ?? "";
    for (const line of pieces) view.lines.push(line.endsWith("\r") ? line.slice(0, -1) : line);
    view.count = view.lines.length;
    emit({ ...view });
    if (read >= MAX_VIEW_BYTES) {
      view.truncated = view.size > read;
      break;
    }
  }
  if (!view.binary && partial !== "") view.lines.push(partial);
  view.count = view.lines.length;
  view.done = true;
  return { ...view };
}
