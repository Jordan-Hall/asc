// Review threads on `asc:` diff editors (SPEC 24.4.7, limits 24.4.12). The model is editor-free so it
// runs under Bun: `ReviewControllers` keeps at most 20 PR controllers (LRU), each loads at most 200
// threads in pages of 50, comment text is capped at 64 KiB, and the threads of a closed diff editor
// are disposed. `registerReviewComments` is the only `vscode` user. Forge calls go through
// `ForgeReviewApi`, which the host-side relay implements; its deny-by-default allowlist
// (`GET pulls/{n}/reviews[/{id}/comments]`, `POST pulls/{n}/reviews` without `event`) is the only
// way in, and no token is ever held here. Submitting a review stays in the panel.
import type * as vscodeTypes from "vscode";
import { AscUriError, parseAscUri } from "../fs/ascfs";

export const MAX_PR_CONTROLLERS = 20;
export const MAX_THREADS_PER_PR = 200;
export const THREAD_PAGE = 50;
export const MAX_COMMENT_BYTES = 64 * 1024;

export interface PrRef {
  /** The bridge root of the asc workspace the PR belongs to. */
  root: string;
  owner: string;
  repo: string;
  number: number;
  /** Full 40-hex ids; line mapping uses these (stacks D8). */
  mergeBase: string;
  head: string;
}

export const prKey = (p: PrRef): string => `${p.owner}/${p.repo}#${p.number}`;

export type Side = "old" | "new";

export interface ForgeComment {
  id: number;
  path: string;
  /** 1-based; 0 when the comment is outdated. */
  line: number;
  side: Side;
  body: string;
  author: string;
  createdAt: string;
  resolved: boolean;
}

export interface CommentPage {
  items: ForgeComment[];
  /** `X-Total-Count`. */
  total: number;
}

export interface PendingComment {
  path: string;
  side: Side;
  line: number;
  body: string;
}

/** What the relay offers; tests supply a fake. TODO(C41b): wire to the relay once it lands. */
export interface ForgeReviewApi {
  listComments(pr: PrRef, page: number, limit: number, signal?: AbortSignal): Promise<CommentPage>;
  /** `POST pulls/{n}/reviews` with no `event`: a pending review the panel submits. */
  savePending(pr: PrRef, comments: PendingComment[]): Promise<void>;
}

export interface ThreadHandle {
  dispose(): void;
}

export interface DisplayComment {
  author: string;
  body: string;
  createdAt: string;
}

/** The editor side of one PR controller. */
export interface ControllerPort {
  createThread(
    file: string,
    query: string,
    line: number,
    comments: DisplayComment[],
    resolved: boolean,
  ): ThreadHandle;
  /** "N more threads, open in the review panel". */
  notice(message: string): void;
  dispose(): void;
}

export interface DiffTarget {
  path: string;
  side: Side;
}

/** Maps an `asc:` document to a PR file and side; null when it is neither the merge base nor the head. */
export function mapUri(pr: PrRef, uriPath: string, query: string): DiffTarget | null {
  let ref;
  try {
    ref = parseAscUri(uriPath, query);
  } catch (e) {
    if (e instanceof AscUriError) return null;
    throw e;
  }
  if (ref.root !== pr.root || ref.at.kind !== "rev") return null;
  if (ref.at.rev === pr.head) return { path: ref.path, side: "new" };
  if (ref.at.rev === pr.mergeBase) return { path: ref.path, side: "old" };
  return null;
}

export function moreThreadsNotice(shown: number, total: number): string {
  const n = total - shown;
  return `${n} more thread${n === 1 ? "" : "s"}, open in the review panel`;
}

export function validateCommentText(text: string): string | null {
  if (text.trim() === "") return "A comment cannot be empty";
  if (new TextEncoder().encode(text).byteLength > MAX_COMMENT_BYTES) {
    return "A comment is limited to 64 KiB";
  }
  return null;
}

/** Groups comments that share a place into one thread. */
export function groupThreads(items: readonly ForgeComment[]): ForgeComment[][] {
  const by = new Map<string, ForgeComment[]>();
  for (const c of items) {
    const k = `${c.path}\u0000${c.side}\u0000${c.line}`;
    const g = by.get(k);
    if (g) g.push(c);
    else by.set(k, [c]);
  }
  return [...by.values()];
}

export class PrReview {
  private readonly threads = new Map<string, ThreadHandle[]>();
  private all: ForgeComment[] = [];
  private total = 0;
  private loaded: Promise<void> | null = null;
  private readonly pending: PendingComment[] = [];
  private disposed = false;

  constructor(
    readonly pr: PrRef,
    private readonly api: ForgeReviewApi,
    private readonly port: ControllerPort,
  ) {}

  /** Loads pages of 50 up to 200 comments; shows the "N more" notice past the cap. */
  load(signal?: AbortSignal): Promise<void> {
    this.loaded ??= this.fetchAll(signal).catch((e) => {
      this.loaded = null;
      throw e;
    });
    return this.loaded;
  }

  private async fetchAll(signal?: AbortSignal): Promise<void> {
    const out: ForgeComment[] = [];
    for (let page = 1; out.length < MAX_THREADS_PER_PR; page++) {
      const r = await this.api.listComments(this.pr, page, THREAD_PAGE, signal);
      this.total = r.total;
      out.push(...r.items);
      if (r.items.length < THREAD_PAGE || out.length >= r.total) break;
    }
    this.all = out.slice(0, MAX_THREADS_PER_PR);
    if (this.total > this.all.length) {
      this.port.notice(moreThreadsNotice(this.all.length, this.total));
    }
  }

  /** A diff editor for the document opened: show the threads that belong to it. */
  async show(uriPath: string, query: string, signal?: AbortSignal): Promise<void> {
    const target = mapUri(this.pr, uriPath, query);
    if (target === null || this.disposed) return;
    const key = `${uriPath}?${query}`;
    if (this.threads.has(key)) return;
    this.threads.set(key, []); // claim the slot before awaiting
    try {
      await this.load(signal);
    } catch (e) {
      this.threads.delete(key);
      throw e;
    }
    if (this.disposed || !this.threads.has(key)) return;
    const handles: ThreadHandle[] = [];
    const mine = this.all.filter(
      (c) => c.path === target.path && c.side === target.side && c.line > 0,
    );
    for (const g of groupThreads(mine)) {
      const first = g[0] as ForgeComment;
      handles.push(
        this.port.createThread(
          uriPath,
          query,
          first.line - 1,
          g.map((c) => ({ author: c.author, body: c.body, createdAt: c.createdAt })),
          g.every((c) => c.resolved),
        ),
      );
    }
    this.threads.set(key, handles);
  }

  /** A diff editor closed: dispose its threads. */
  close(uriPath: string, query: string): void {
    const key = `${uriPath}?${query}`;
    for (const h of this.threads.get(key) ?? []) h.dispose();
    this.threads.delete(key);
  }

  /** Queues a new comment for the pending review. Returns an error message or null. */
  add(uriPath: string, query: string, line0: number, body: string): string | null {
    const target = mapUri(this.pr, uriPath, query);
    if (target === null) return "Comments go on the base or head version of a file in this PR";
    const bad = validateCommentText(body);
    if (bad !== null) return bad;
    this.pending.push({ path: target.path, side: target.side, line: line0 + 1, body });
    return null;
  }

  pendingCount(): number {
    return this.pending.length;
  }

  /** Saves the queue as one pending review; the panel submits it. */
  async savePending(): Promise<number> {
    if (this.pending.length === 0) return 0;
    const batch = this.pending.slice();
    await this.api.savePending(this.pr, batch);
    this.pending.splice(0, batch.length);
    return batch.length;
  }

  dispose(): void {
    if (this.disposed) return;
    this.disposed = true;
    for (const hs of this.threads.values()) for (const h of hs) h.dispose();
    this.threads.clear();
    this.port.dispose();
  }
}

/** At most 20 PR controllers; the least recently used is disposed. */
export class ReviewControllers {
  private readonly map = new Map<string, PrReview>();
  constructor(
    private readonly api: ForgeReviewApi,
    private readonly makePort: (pr: PrRef) => ControllerPort,
    private readonly max = MAX_PR_CONTROLLERS,
  ) {}

  open(pr: PrRef): PrReview {
    const k = prKey(pr);
    const have = this.map.get(k);
    if (have) {
      this.map.delete(k);
      this.map.set(k, have);
      return have;
    }
    const made = new PrReview(pr, this.api, this.makePort(pr));
    this.map.set(k, made);
    while (this.map.size > this.max) {
      const oldest = this.map.keys().next().value as string;
      this.map.get(oldest)?.dispose();
      this.map.delete(oldest);
    }
    return made;
  }

  size(): number {
    return this.map.size;
  }

  /** The most recently used controller whose PR owns an `asc:` document. */
  forUri(uriPath: string, query: string): PrReview | null {
    for (const r of [...this.map.values()].toReversed()) {
      if (mapUri(r.pr, uriPath, query) !== null) return r;
    }
    return null;
  }

  dispose(): void {
    for (const r of this.map.values()) r.dispose();
    this.map.clear();
  }
}

// --- the vscode adapter ---------------------------------------------------------------------------

export interface ReviewRegistration {
  dispose(): void;
  /** Opens a PR's controller; the review panel calls this with the PR it shows. */
  openPr(pr: PrRef): PrReview;
}

export function registerReviewComments(
  vscode: typeof vscodeTypes,
  api: ForgeReviewApi,
  log: (line: string) => void,
): ReviewRegistration {
  const subs: vscodeTypes.Disposable[] = [];
  const reviews = new ReviewControllers(api, (pr) => {
    const ctl = vscode.comments.createCommentController("asc.review", `asc review ${prKey(pr)}`);
    ctl.commentingRangeProvider = {
      provideCommentingRanges: (doc) =>
        doc.uri.scheme === "asc" && mapUri(pr, doc.uri.path, doc.uri.query) !== null
          ? [new vscode.Range(0, 0, Math.max(0, doc.lineCount - 1), 0)]
          : [],
    };
    return {
      createThread(file, query, line, comments, resolved) {
        const uri = vscode.Uri.from({ scheme: "asc", path: file, query });
        const t = ctl.createCommentThread(
          uri,
          new vscode.Range(line, 0, line, 0),
          comments.map((c) => ({
            body: new vscode.MarkdownString(c.body),
            mode: vscode.CommentMode.Preview,
            author: { name: c.author },
            timestamp: new Date(c.createdAt),
          })),
        );
        t.state = resolved
          ? vscode.CommentThreadState.Resolved
          : vscode.CommentThreadState.Unresolved;
        t.canReply = true;
        return t;
      },
      notice: (m) => log(m),
      dispose: () => ctl.dispose(),
    };
  });
  const handle = (doc: vscodeTypes.TextDocument): void => {
    if (doc.uri.scheme !== "asc") return;
    reviews
      .forUri(doc.uri.path, doc.uri.query)
      ?.show(doc.uri.path, doc.uri.query)
      .catch((e) => log(`review threads failed: ${String(e)}`));
  };
  subs.push(
    vscode.workspace.onDidOpenTextDocument(handle),
    vscode.workspace.onDidCloseTextDocument((doc) => {
      if (doc.uri.scheme !== "asc") return;
      reviews.forUri(doc.uri.path, doc.uri.query)?.close(doc.uri.path, doc.uri.query);
    }),
    vscode.commands.registerCommand("asc.review.createComment", (reply: vscodeTypes.CommentReply) => {
      const uri = reply.thread.uri;
      const r = reviews.forUri(uri.path, uri.query);
      if (!r) return;
      const err = r.add(uri.path, uri.query, reply.thread.range?.start.line ?? 0, reply.text);
      if (err !== null) void vscode.window.showWarningMessage(err);
      else {
        void vscode.window.showInformationMessage(
          `${r.pendingCount()} pending comment(s); submit from the review panel`,
        );
      }
    }),
  );
  for (const d of vscode.workspace.textDocuments) handle(d);
  return {
    openPr: (pr) => reviews.open(pr),
    dispose() {
      for (const s of subs) s.dispose();
      reviews.dispose();
    },
  };
}
