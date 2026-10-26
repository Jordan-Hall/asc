// Review threads on `asc:` diff editors (SPEC 24.4.7, limits 24.4.12), over the host-neutral
// `asc.v1.Review` service (feature `review.v1`): asc stores the threads itself, so every client sees the
// same ones whichever forge, if any, sits in front of the server. The model is editor-free so it runs
// under Bun: `ReviewControllers` keeps at most 20 change controllers (LRU), each loads at most 200
// threads in pages of 50, comment text is capped at 64 KiB, and the threads of a closed diff editor
// are disposed. `registerReviewComments` is the only `vscode` user. A comment is posted at once; the
// threads are re-read afterwards, so what the editor shows is what the server holds.
import { callUnary, changeIdFromString, changeIdToString, toHex } from "@asc/asc-client";
import { DiffSide, Review, ThreadStatus } from "@asc/asc-client/review";
import { Workspace } from "@asc/asc-client/workspace";
import type * as vscodeTypes from "vscode";
import type { BridgeManager } from "../bridge";
import { wsIdFor } from "../views/worktrees";
import { AscUriError, parseAscUri } from "../fs/ascfs";

export const MAX_CONTROLLERS = 20;
export const MAX_THREADS_PER_CHANGE = 200;
export const THREAD_PAGE = 50;
export const MAX_COMMENT_BYTES = 64 * 1024;

export interface ChangeRef {
  /** The bridge root of the asc workspace the change belongs to. */
  root: string;
  /** `owner/repo` on the asc server. */
  repo: string;
  /** The change id, 32 letters k-z. */
  change: string;
  /** Full 40-hex ids; line mapping uses these (stacks D8). */
  mergeBase: string;
  head: string;
}

export const changeKey = (c: ChangeRef): string => `${c.repo}@${c.change}`;

export type Side = "old" | "new";

export interface ThreadComment {
  id: number;
  author: string;
  body: string;
  /** ISO 8601. */
  createdAt: string;
}

export interface ThreadRow {
  id: number;
  path: string;
  /** The change version the thread is anchored to. */
  version: number;
  side: Side;
  /** 1-based; 0 for a thread on the change as a whole. */
  line: number;
  resolved: boolean;
  comments: ThreadComment[];
}

export interface ThreadPage {
  items: ThreadRow[];
  /** Empty on the last page. */
  next: string;
}

export interface NewComment {
  /** Reply to this thread; 0 or absent starts a thread at `path`/`side`/`line`. */
  threadId?: number;
  path: string;
  side: Side;
  /** 1-based. */
  line: number;
  body: string;
}

/** What the bridge offers; tests supply a fake. */
export interface ReviewApi {
  listThreads(
    c: ChangeRef,
    pageToken: string,
    limit: number,
    signal?: AbortSignal,
  ): Promise<ThreadPage>;
  addComment(c: ChangeRef, n: NewComment): Promise<void>;
  resolveThread(c: ChangeRef, threadId: number, resolved: boolean): Promise<void>;
  requestReview(c: ChangeRef, add: string[], remove: string[]): Promise<void>;
}

export interface ThreadHandle {
  dispose(): void;
}

export interface DisplayComment {
  author: string;
  body: string;
  createdAt: string;
}

export interface ThreadMeta {
  id: number;
  /** "version 2", shown beside the thread; empty when unknown. */
  label: string;
}

/** The editor side of one change controller. */
export interface ControllerPort {
  createThread(
    file: string,
    query: string,
    line: number,
    comments: DisplayComment[],
    resolved: boolean,
    meta: ThreadMeta,
  ): ThreadHandle;
  /** "More threads, open in the review panel". */
  notice(message: string): void;
  dispose(): void;
}

export interface DiffTarget {
  path: string;
  side: Side;
}

/** Maps an `asc:` document to a change file and side; null when it is neither the merge base nor the head. */
export function mapUri(c: ChangeRef, uriPath: string, query: string): DiffTarget | null {
  let ref;
  try {
    ref = parseAscUri(uriPath, query);
  } catch (e) {
    if (e instanceof AscUriError) return null;
    throw e;
  }
  if (ref.root !== c.root || ref.at.kind !== "rev") return null;
  if (ref.at.rev === c.head) return { path: ref.path, side: "new" };
  if (ref.at.rev === c.mergeBase) return { path: ref.path, side: "old" };
  return null;
}

export function moreThreadsNotice(shown: number): string {
  return `Showing the first ${shown} threads; open the review panel for the rest`;
}

export function validateCommentText(text: string): string | null {
  if (text.trim() === "") return "A comment cannot be empty";
  if (new TextEncoder().encode(text).byteLength > MAX_COMMENT_BYTES) {
    return "A comment is limited to 64 KiB";
  }
  return null;
}

interface Shown {
  uriPath: string;
  query: string;
  handles: ThreadHandle[];
}

export class ChangeReview {
  private readonly threads = new Map<string, Shown>();
  private all: ThreadRow[] = [];
  private loaded: Promise<void> | null = null;
  private disposed = false;

  constructor(
    readonly ref: ChangeRef,
    private readonly api: ReviewApi,
    private readonly port: ControllerPort,
  ) {}

  /** Loads pages of 50 up to 200 threads; shows the "more" notice past the cap. */
  load(signal?: AbortSignal): Promise<void> {
    this.loaded ??= this.fetchAll(signal).catch((e) => {
      this.loaded = null;
      throw e;
    });
    return this.loaded;
  }

  private async fetchAll(signal?: AbortSignal): Promise<void> {
    const out: ThreadRow[] = [];
    let token = "";
    do {
      const r = await this.api.listThreads(this.ref, token, THREAD_PAGE, signal);
      out.push(...r.items);
      token = r.next;
    } while (token !== "" && out.length < MAX_THREADS_PER_CHANGE);
    this.all = out.slice(0, MAX_THREADS_PER_CHANGE);
    if (token !== "" || out.length > MAX_THREADS_PER_CHANGE) {
      this.port.notice(moreThreadsNotice(this.all.length));
    }
  }

  /** A diff editor for the document opened: show the threads that belong to it. */
  async show(uriPath: string, query: string, signal?: AbortSignal): Promise<void> {
    const target = mapUri(this.ref, uriPath, query);
    if (target === null || this.disposed) return;
    const key = `${uriPath}?${query}`;
    if (this.threads.has(key)) return;
    const slot: Shown = { uriPath, query, handles: [] };
    this.threads.set(key, slot); // claim the slot before awaiting
    try {
      await this.load(signal);
    } catch (e) {
      this.threads.delete(key);
      throw e;
    }
    if (this.disposed || this.threads.get(key) !== slot) return;
    for (const t of this.all) {
      if (t.path !== target.path || t.side !== target.side || t.line <= 0) continue;
      slot.handles.push(
        this.port.createThread(
          uriPath,
          query,
          t.line - 1,
          t.comments.map((c) => ({ author: c.author, body: c.body, createdAt: c.createdAt })),
          t.resolved,
          { id: t.id, label: t.version > 0 ? `version ${t.version}` : "" },
        ),
      );
    }
  }

  /** A diff editor closed: dispose its threads. */
  close(uriPath: string, query: string): void {
    const key = `${uriPath}?${query}`;
    for (const h of this.threads.get(key)?.handles ?? []) h.dispose();
    this.threads.delete(key);
  }

  /** Re-reads the threads and redraws every open document. */
  async reload(): Promise<void> {
    const open = [...this.threads.values()];
    for (const v of open) for (const h of v.handles) h.dispose();
    this.threads.clear();
    this.loaded = null;
    for (const v of open) await this.show(v.uriPath, v.query);
  }

  /** Posts a comment (a reply when `threadId` is given). Returns an error message or null. */
  async add(
    uriPath: string,
    query: string,
    line0: number,
    body: string,
    threadId = 0,
  ): Promise<string | null> {
    const target = mapUri(this.ref, uriPath, query);
    if (target === null) return "Comments go on the base or head version of a file in this change";
    const bad = validateCommentText(body);
    if (bad !== null) return bad;
    await this.api.addComment(this.ref, {
      ...(threadId > 0 ? { threadId } : {}),
      path: target.path,
      side: target.side,
      line: line0 + 1,
      body,
    });
    await this.reload();
    return null;
  }

  async resolve(threadId: number, resolved: boolean): Promise<void> {
    await this.api.resolveThread(this.ref, threadId, resolved);
    await this.reload();
  }

  async requestReview(add: string[], remove: string[] = []): Promise<void> {
    await this.api.requestReview(this.ref, add, remove);
  }

  dispose(): void {
    if (this.disposed) return;
    this.disposed = true;
    for (const v of this.threads.values()) for (const h of v.handles) h.dispose();
    this.threads.clear();
    this.port.dispose();
  }
}

/** At most 20 change controllers; the least recently used is disposed. */
export class ReviewControllers {
  private readonly map = new Map<string, ChangeReview>();
  constructor(
    private readonly api: ReviewApi,
    private readonly makePort: (c: ChangeRef) => ControllerPort,
    private readonly max = MAX_CONTROLLERS,
  ) {}

  open(c: ChangeRef): ChangeReview {
    const k = changeKey(c);
    const have = this.map.get(k);
    if (have) {
      this.map.delete(k);
      this.map.set(k, have);
      return have;
    }
    const made = new ChangeReview(c, this.api, this.makePort(c));
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

  /** The most recently used controller whose change owns an `asc:` document. */
  forUri(uriPath: string, query: string): ChangeReview | null {
    for (const r of [...this.map.values()].toReversed()) {
      if (mapUri(r.ref, uriPath, query) !== null) return r;
    }
    return null;
  }

  dispose(): void {
    for (const r of this.map.values()) r.dispose();
    this.map.clear();
  }
}

// --- the bridge adapter ---------------------------------------------------------------------------

const enc = new TextEncoder();
const dec = new TextDecoder();

const sideOf = (s: DiffSide): Side => (s === DiffSide.OLD ? "old" : "new");

export function bridgeReviewApi(mgr: BridgeManager): ReviewApi {
  return {
    async listThreads(c, pageToken, limit, signal) {
      const r = await callUnary(
        mgr.node(),
        Review.method.listThreads,
        {
          repo: { name: c.repo },
          change: changeIdFromString(c.change),
          limit,
          pageToken,
        },
        { root: c.root, ...(signal ? { signal } : {}) },
      );
      return {
        next: r.nextPageToken,
        items: r.threads.map((t) => ({
          id: Number(t.id),
          path: t.anchor ? dec.decode(t.anchor.path) : "",
          version: t.anchor?.version ?? 0,
          side: sideOf(t.anchor?.side ?? DiffSide.NEW),
          line: t.anchor?.startLine ?? 0,
          resolved: t.status === ThreadStatus.RESOLVED,
          comments: t.comments.map((m) => ({
            id: Number(m.id),
            author: m.author,
            body: m.body,
            createdAt: new Date(Number(m.created) * 1000).toISOString(),
          })),
        })),
      };
    },

    async addComment(c, n) {
      await callUnary(
        mgr.node(),
        Review.method.addComment,
        {
          repo: { name: c.repo },
          change: changeIdFromString(c.change),
          threadId: BigInt(n.threadId ?? 0),
          anchor: {
            path: enc.encode(n.path),
            version: 0,
            side: n.side === "old" ? DiffSide.OLD : DiffSide.NEW,
            startLine: n.line,
            endLine: n.line,
          },
          body: n.body,
        },
        { root: c.root },
      );
    },

    async resolveThread(c, threadId, resolved) {
      await callUnary(
        mgr.node(),
        Review.method.resolveThread,
        { repo: { name: c.repo }, threadId: BigInt(threadId), resolved },
        { root: c.root },
      );
    },

    async requestReview(c, add, remove) {
      await callUnary(
        mgr.node(),
        Review.method.requestReview,
        { repo: { name: c.repo }, change: changeIdFromString(c.change), add, remove },
        { root: c.root },
      );
    },
  };
}

/** The current change of an asc-server root as a `ChangeRef`: head is its commit, the merge base is its
 * stack parent's commit (the stack base for the first change). Undefined when the working copy is on no stack. */
export async function currentChangeRef(
  mgr: BridgeManager,
  root: string,
  repo: string,
): Promise<ChangeRef | undefined> {
  const wsId = await wsIdFor(mgr, root);
  const r = await callUnary(mgr.node(), Workspace.method.stacks, { wsId }, { root });
  for (const st of r.stacks) {
    const cur = st.changes.find((c) => c.current);
    if (!cur) continue;
    const parentId = cur.parentChange.length > 0 ? changeIdToString(cur.parentChange) : "";
    const parent = st.changes.find((c) => changeIdToString(c.change) === parentId);
    return {
      root,
      repo,
      change: changeIdToString(cur.change),
      head: toHex(cur.commit),
      mergeBase: toHex(parent ? parent.commit : st.base),
    };
  }
  return undefined;
}

// --- the vscode adapter ---------------------------------------------------------------------------

export interface ReviewHost {
  /** The current change of a root; undefined when the root has no asc server or no current change. */
  current(root: string): Promise<ChangeRef | undefined>;
}

export interface ReviewRegistration {
  dispose(): void;
  /** Opens a change's controller; the diff editors of its head and merge base then show its threads. */
  open(c: ChangeRef): ChangeReview;
}

export function registerReviewComments(
  vscode: typeof vscodeTypes,
  api: ReviewApi,
  host: ReviewHost,
  roots: readonly string[],
  log: (line: string) => void,
): ReviewRegistration {
  const subs: vscodeTypes.Disposable[] = [];
  const ctl = vscode.comments.createCommentController("asc.review", "asc review");
  subs.push(ctl);
  const ids = new WeakMap<object, number>();
  const owned = new Set<(uriPath: string, query: string) => boolean>();
  ctl.commentingRangeProvider = {
    provideCommentingRanges: (doc) =>
      doc.uri.scheme === "asc" && [...owned].some((f) => f(doc.uri.path, doc.uri.query))
        ? [new vscode.Range(0, 0, Math.max(0, doc.lineCount - 1), 0)]
        : [],
  };

  const reviews = new ReviewControllers(api, (c) => {
    const mine = (p: string, q: string) => mapUri(c, p, q) !== null;
    owned.add(mine);
    return {
      createThread(file, query, line, comments, resolved, meta) {
        const uri = vscode.Uri.from({ scheme: "asc", path: file, query });
        const t = ctl.createCommentThread(
          uri,
          new vscode.Range(line, 0, line, 0),
          comments.map((m) => ({
            body: new vscode.MarkdownString(m.body),
            mode: vscode.CommentMode.Preview,
            author: { name: m.author },
            timestamp: new Date(m.createdAt),
          })),
        );
        t.state = resolved
          ? vscode.CommentThreadState.Resolved
          : vscode.CommentThreadState.Unresolved;
        t.contextValue = resolved ? "asc.thread.resolved" : "asc.thread.open";
        if (meta.label !== "") t.label = meta.label;
        t.canReply = true;
        ids.set(t, meta.id);
        return t;
      },
      notice: (m) => log(m),
      dispose: () => void owned.delete(mine),
    };
  });
  const handle = (doc: vscodeTypes.TextDocument): void => {
    if (doc.uri.scheme !== "asc") return;
    reviews
      .forUri(doc.uri.path, doc.uri.query)
      ?.show(doc.uri.path, doc.uri.query)
      .catch((e) => log(`review threads failed: ${String(e)}`));
  };
  const fail = (what: string) => (e: unknown) => {
    log(`asc review ${what}: ${String(e)}`);
    void vscode.window.showErrorMessage(`ASC: could not ${what}.`);
  };
  const pickRoot = async (): Promise<string | undefined> =>
    roots.length === 1
      ? roots[0]
      : vscode.window.showQuickPick([...roots], { placeHolder: "Choose an ASC root" });
  const openCurrent = async (): Promise<ChangeReview | undefined> => {
    const root = await pickRoot();
    if (!root) return undefined;
    const ref = await host.current(root);
    if (!ref) {
      void vscode.window.showInformationMessage(
        "ASC: the working copy is not on a change with an ASC server.",
      );
      return undefined;
    }
    const r = reviews.open(ref);
    for (const d of vscode.workspace.textDocuments) handle(d);
    return r;
  };

  subs.push(
    vscode.workspace.onDidOpenTextDocument(handle),
    vscode.workspace.onDidCloseTextDocument((doc) => {
      if (doc.uri.scheme !== "asc") return;
      reviews.forUri(doc.uri.path, doc.uri.query)?.close(doc.uri.path, doc.uri.query);
    }),
    vscode.commands.registerCommand("asc.review.showThreads", async () => {
      try {
        const r = await openCurrent();
        if (r) {
          void vscode.window.showInformationMessage(
            "ASC: review threads show in the diff editors of this change's base and head.",
          );
        }
      } catch (e) {
        fail("show review threads")(e);
      }
    }),
    vscode.commands.registerCommand("asc.review.requestReview", async () => {
      try {
        const r = await openCurrent();
        if (!r) return;
        const text = await vscode.window.showInputBox({
          prompt: "Reviewer handles, separated by commas",
          validateInput: (v) =>
            /^\s*[\w.-]{1,40}(\s*,\s*[\w.-]{1,40}){0,15}\s*$/.test(v)
              ? undefined
              : "Enter 1 to 16 handles",
        });
        if (!text) return;
        await r.requestReview(text.split(",").map((h) => h.trim()));
        void vscode.window.showInformationMessage("ASC: review requested.");
      } catch (e) {
        fail("request a review")(e);
      }
    }),
    vscode.commands.registerCommand(
      "asc.review.createComment",
      async (reply: vscodeTypes.CommentReply) => {
        const uri = reply.thread.uri;
        const r = reviews.forUri(uri.path, uri.query);
        if (!r) return;
        const existing = ids.get(reply.thread) ?? 0;
        try {
          const err = await r.add(
            uri.path,
            uri.query,
            reply.thread.range?.start.line ?? 0,
            reply.text,
            existing,
          );
          if (err !== null) void vscode.window.showWarningMessage(err);
          else if (existing === 0) reply.thread.dispose(); // the draft; the re-read drew the real thread
        } catch (e) {
          fail("post the comment")(e);
        }
      },
    ),
    ...(["resolve", "reopen"] as const).map((verb) =>
      vscode.commands.registerCommand(
        `asc.review.${verb}Thread`,
        async (thread?: vscodeTypes.CommentThread) => {
          const id = thread ? ids.get(thread) : undefined;
          if (!thread || id === undefined) return;
          try {
            await reviews
              .forUri(thread.uri.path, thread.uri.query)
              ?.resolve(id, verb === "resolve");
          } catch (e) {
            fail(`${verb} the thread`)(e);
          }
        },
      ),
    ),
  );
  for (const d of vscode.workspace.textDocuments) handle(d);
  return {
    open: (c) => reviews.open(c),
    dispose() {
      for (const s of subs) s.dispose();
      reviews.dispose();
    },
  };
}
