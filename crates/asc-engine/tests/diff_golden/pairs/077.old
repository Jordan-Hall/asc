// `asc-actions:` read-only log documents (SPEC 24.4.7). A job log opens as the last 64 KiB read with
// one suffix range request; "load earlier" reads the 64 KiB before what is loaded. Bytes are decoded
// as UTF-8 with terminal escapes and control characters removed, so server text cannot drive the
// editor. Editor-free: `registerCi` (tasks/views) is the only `vscode` user.

export const SCHEME = "asc-actions";
export const TAIL_BYTES = 64 * 1024;
export const CHUNK_BYTES = 64 * 1024;
/** Most bytes one document holds; "load earlier" stops here. */
export const MAX_LOADED_BYTES = 8 * 1024 * 1024;

export type LogRangeRequest = { suffix: number } | { start: number; end: number };

export interface LogRange {
  bytes: Uint8Array;
  /** Offset of `bytes[0]` in the whole log. */
  start: number;
  /** Size of the whole log. */
  total: number;
}

export interface LogReadApi {
  readRange(jobId: number, range: LogRangeRequest, signal?: AbortSignal): Promise<LogRange>;
}

export class ActionsUriError extends Error {}

const NAME = /^(?!\.{1,2}$)[A-Za-z0-9_.-]{1,100}$/;

export function toActionsUri(o: { owner: string; repo: string; jobId: number }): {
  scheme: string;
  path: string;
} {
  if (!NAME.test(o.owner) || !NAME.test(o.repo) || !Number.isSafeInteger(o.jobId) || o.jobId < 0) {
    throw new ActionsUriError("asc-actions: bad owner, repo or job");
  }
  return { scheme: SCHEME, path: `/${o.owner}/${o.repo}/job/${o.jobId}.log` };
}

export function parseActionsUri(path: string): { owner: string; repo: string; jobId: number } {
  const m = /^\/([^/]+)\/([^/]+)\/job\/(\d{1,15})\.log$/.exec(path);
  if (!m || !NAME.test(m[1] ?? "") || !NAME.test(m[2] ?? "")) {
    throw new ActionsUriError("asc-actions: uri is not /<owner>/<repo>/job/<id>.log");
  }
  return { owner: m[1] as string, repo: m[2] as string, jobId: Number(m[3]) };
}

// CSI and OSC sequences, then C0 controls other than tab and newline.
const ANSI =
  // oxlint-disable-next-line no-control-regex
  /\u001b\[[0-9;?]*[ -/]*[@-~]|\u001b\][^\u0007]*(?:\u0007|\u001b\\)|[\u0000-\u0008\u000b\u000c\u000e-\u001f\u007f]/g;

export function sanitizeLog(raw: string): string {
  return raw.replace(/\r\n?/g, "\n").replace(ANSI, "");
}

/** One log document: the bytes loaded so far, as a tail that grows backwards. */
export class ActionsLogModel {
  private buf: Uint8Array = new Uint8Array(0);
  private start = 0;
  private total = 0;
  private loaded = false;

  constructor(
    readonly jobId: number,
    private readonly api: LogReadApi,
  ) {}

  get hasEarlier(): boolean {
    return this.loaded && this.start > 0 && this.buf.length < MAX_LOADED_BYTES;
  }

  /** Reads the last `TAIL_BYTES` again; used for the first open and for a running job. */
  async loadTail(signal?: AbortSignal): Promise<void> {
    const r = await this.api.readRange(this.jobId, { suffix: TAIL_BYTES }, signal);
    this.buf = r.bytes;
    this.start = r.start;
    this.total = r.total;
    this.loaded = true;
  }

  async loadEarlier(signal?: AbortSignal): Promise<void> {
    if (!this.loaded) return this.loadTail(signal);
    if (!this.hasEarlier) return;
    const end = this.start - 1;
    const start = Math.max(0, this.start - CHUNK_BYTES);
    const r = await this.api.readRange(this.jobId, { start, end }, signal);
    if (r.start + r.bytes.length !== this.start) return; // the log changed under us: keep what we have
    const next = new Uint8Array(r.bytes.length + this.buf.length);
    next.set(r.bytes, 0);
    next.set(this.buf, r.bytes.length);
    this.buf = next;
    this.start = r.start;
    this.total = r.total;
  }

  text(): string {
    let from = 0;
    // A range can start inside a UTF-8 sequence: drop its continuation bytes.
    if (this.start > 0) while (from < this.buf.length && ((this.buf[from] ?? 0) & 0xc0) === 0x80) from++;
    const body = sanitizeLog(new TextDecoder("utf-8").decode(this.buf.subarray(from)));
    if (!this.loaded) return "";
    if (this.start === 0) return body;
    const hidden = this.start + from;
    return `... ${hidden} earlier bytes not shown (run "asc: Load earlier log output")\n${body}`;
  }
}

/** The documents behind the `asc-actions:` scheme. `resolve` maps `owner/repo` to a reader. */
export class ActionsLogDocs {
  private readonly models = new Map<string, ActionsLogModel>();
  constructor(private readonly resolve: (owner: string, repo: string) => LogReadApi | undefined) {}

  private model(path: string): ActionsLogModel {
    const p = parseActionsUri(path);
    const known = this.models.get(path);
    if (known) return known;
    const api = this.resolve(p.owner, p.repo);
    if (!api) throw new ActionsUriError("asc-actions: no signed-in forge for this repository");
    const m = new ActionsLogModel(p.jobId, api);
    this.models.set(path, m);
    return m;
  }

  /** Content for a document; the first read loads the tail, later reads reuse what is loaded. */
  async provide(path: string, signal?: AbortSignal): Promise<string> {
    const m = this.model(path);
    if (m.text() === "") await m.loadTail(signal);
    return m.text();
  }

  async loadEarlier(path: string, signal?: AbortSignal): Promise<string> {
    const m = this.model(path);
    await m.loadEarlier(signal);
    return m.text();
  }

  /** Re-reads the tail (a running job) and returns the new text. */
  async reload(path: string, signal?: AbortSignal): Promise<string> {
    const m = this.model(path);
    await m.loadTail(signal);
    return m.text();
  }

  close(path: string): void {
    this.models.delete(path);
  }
}
