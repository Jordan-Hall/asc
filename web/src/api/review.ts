// Review threads over asc.v1.Review (the `review.v1` feature). Reads only; the write calls
// (AddComment, ResolveThread, RequestReview) arrive with the review screens in v1.2.
import {
  type BridgeTransport,
  callUnary,
  type CallParams,
  changeIdFromString,
} from "@asc/asc-client";
import { Review, ThreadStatus } from "@asc/asc-client/review";

export type { ThreadStatus };

export interface ThreadComment {
  id: number;
  author: string;
  body: string;
  /** Seconds since the epoch. */
  created: number;
}

export interface ThreadView {
  id: number;
  /** Repository path of the anchor; empty for a thread on the change as a whole. */
  path: string;
  version: number;
  startLine: number;
  resolved: boolean;
  openedBy: string;
  comments: readonly ThreadComment[];
}

export interface ThreadsView {
  threads: readonly ThreadView[];
  openThreads: number;
  next: string;
}

const text = new TextDecoder();

export async function listThreads(
  t: BridgeTransport,
  repo: string,
  change: string,
  opts: { status?: ThreadStatus; version?: number; pageToken?: string } = {},
  p?: CallParams,
): Promise<ThreadsView> {
  const res = await callUnary(
    t,
    Review.method.listThreads,
    {
      repo: { name: repo },
      change: changeIdFromString(change),
      status: opts.status ?? ThreadStatus.UNSPECIFIED,
      version: opts.version ?? 0,
      pageToken: opts.pageToken ?? "",
    },
    p,
  );
  return {
    openThreads: res.openThreads,
    next: res.nextPageToken,
    threads: res.threads.map((th) => ({
      id: Number(th.id),
      path: th.anchor ? text.decode(th.anchor.path) : "",
      version: th.anchor?.version ?? 0,
      startLine: th.anchor?.startLine ?? 0,
      resolved: th.status === ThreadStatus.RESOLVED,
      openedBy: th.openedBy,
      comments: th.comments.map((c) => ({
        id: Number(c.id),
        author: c.author,
        body: c.body,
        created: Number(c.created),
      })),
    })),
  };
}
