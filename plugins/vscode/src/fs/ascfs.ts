// The read-only `asc:` file system (SPEC 24.4.2): `asc:/<path>?root=<ws>&rev=<sha>`, `&stage=<0-3>` or
// `&file=<FileId>`. Content comes from `Workspace.ReadFile`, which works offline from the local store
// and never reads the working tree. Immutable content (a full commit id or a FileId) is cached in an
// LRU by id (32 MiB); stage and branch-relative content is never cached and stage reads are also
// invalidated by KIND_STAGE through `invalidateStage`.

export const CACHE_BYTES = 32 * 1024 * 1024;

export type AscAt =
  | { kind: "rev"; rev: string }
  | { kind: "stage"; stage: 0 | 1 | 2 | 3 }
  | { kind: "file"; fileId: string };

export interface AscRef {
  /** Repo-relative path with forward slashes, no leading slash. */
  path: string;
  /** The workspace root (the bridge `root`). */
  root: string;
  at: AscAt;
}

/** The bridge calls the file system needs; tests supply a fake. */
export interface ReadApi {
  readFile(root: string, path: string, at: AscAt, signal?: AbortSignal): Promise<Uint8Array>;
}

export class AscUriError extends Error {}

/** Builds `asc:/<path>?root=...&rev=...` as a string (the host wraps it in a Uri). */
export function toAscUri(ref: AscRef): { path: string; query: string } {
  const q = new URLSearchParams();
  q.set("root", ref.root);
  if (ref.at.kind === "rev") q.set("rev", ref.at.rev);
  else if (ref.at.kind === "stage") q.set("stage", String(ref.at.stage));
  else q.set("file", ref.at.fileId);
  return { path: `/${ref.path.replace(/^\/+/, "")}`, query: q.toString() };
}

const HEX40 = /^[0-9a-f]{40}$/;
const HEX = /^[0-9a-f]+$/;

export function parseAscUri(path: string, query: string): AscRef {
  const q = new URLSearchParams(query);
  const root = q.get("root");
  if (root === null || root === "") throw new AscUriError("asc: uri needs a root");
  const rel = path.replace(/^\/+/, "");
  if (rel.split("/").includes("..")) throw new AscUriError("asc: uri path escapes the repository");
  const rev = q.get("rev");
  const stage = q.get("stage");
  const file = q.get("file");
  const given = [rev, stage, file].filter((x) => x !== null).length;
  if (given !== 1) throw new AscUriError("asc: uri needs exactly one of rev, stage or file");
  if (rev !== null) return { path: rel, root, at: { kind: "rev", rev } };
  if (stage !== null) {
    if (!/^[0-3]$/.test(stage)) throw new AscUriError("asc: stage must be 0-3");
    return { path: rel, root, at: { kind: "stage", stage: Number(stage) as 0 | 1 | 2 | 3 } };
  }
  if (file === null || !HEX.test(file)) throw new AscUriError("asc: file id must be hex");
  return { path: rel, root, at: { kind: "file", fileId: file } };
}

/** A key when the content can never change, else null. */
export function immutableKey(ref: AscRef): string | null {
  if (ref.at.kind === "file") return `f:${ref.root}:${ref.at.fileId}`;
  if (ref.at.kind === "rev" && HEX40.test(ref.at.rev)) {
    return `r:${ref.root}:${ref.at.rev}:${ref.path}`;
  }
  return null;
}

/** Byte-bounded LRU. A value larger than the cap is not stored. */
export class ByteLru {
  private readonly map = new Map<string, Uint8Array>();
  private used = 0;
  constructor(private readonly cap: number = CACHE_BYTES) {}
  get bytes(): number {
    return this.used;
  }
  get size(): number {
    return this.map.size;
  }
  get(key: string): Uint8Array | undefined {
    const v = this.map.get(key);
    if (v === undefined) return undefined;
    this.map.delete(key);
    this.map.set(key, v);
    return v;
  }
  set(key: string, value: Uint8Array): void {
    const old = this.map.get(key);
    if (old !== undefined) {
      this.used -= old.byteLength;
      this.map.delete(key);
    }
    if (value.byteLength > this.cap) return;
    this.map.set(key, value);
    this.used += value.byteLength;
    for (const [k, v] of this.map) {
      if (this.used <= this.cap) break;
      this.map.delete(k);
      this.used -= v.byteLength;
    }
  }
}

export interface AscReader {
  read(ref: AscRef, signal?: AbortSignal): Promise<Uint8Array>;
  /** KIND_STAGE hook: stage reads are never cached, so this is a no-op kept for the contract. */
  invalidateStage(root: string): void;
}

export function createAscReader(api: ReadApi, cache: ByteLru = new ByteLru()): AscReader {
  const inflight = new Map<string, Promise<Uint8Array>>();
  return {
    read(ref, signal) {
      const key = immutableKey(ref);
      if (key === null) return api.readFile(ref.root, ref.path, ref.at, signal);
      const hit = cache.get(key);
      if (hit !== undefined) return Promise.resolve(hit);
      const pending = inflight.get(key);
      if (pending !== undefined) return pending;
      const p = api
        .readFile(ref.root, ref.path, ref.at, signal)
        .then((data) => {
          cache.set(key, data);
          return data;
        })
        .finally(() => inflight.delete(key));
      inflight.set(key, p);
      return p;
    },
    invalidateStage() {
      // Stage reads bypass the cache (immutableKey is null for them), so nothing to drop.
    },
  };
}
