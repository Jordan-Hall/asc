// The caller's inbox over asc.v1.Inbox (the `inbox.v1` feature). Reads only.
import {
  type BridgeTransport,
  callUnary,
  type CallParams,
  changeIdToString,
} from "@asc/asc-client";
import { Inbox, type InboxSection } from "@asc/asc-client/inbox";

export type { InboxSection };

export interface InboxItemView {
  change: string;
  section: InboxSection;
  latestVersion: number;
  author: string;
  /** Seconds since the epoch. */
  updated: number;
  openThreads: number;
  pinned: boolean;
  /** Who asked for the review (NEEDS_REVIEW only). */
  requestedBy: string;
}

export interface InboxView {
  items: readonly InboxItemView[];
  total: number;
  next: string;
}

export interface InboxCounts {
  total: number;
  bySection: ReadonlyMap<InboxSection, number>;
  etag: string;
  /** The `ifNoneMatch` etag still matches: nothing changed, and the counts are empty. */
  notModified: boolean;
}

export async function listInbox(
  t: BridgeTransport,
  repo: string,
  opts: { sections?: InboxSection[]; pageToken?: string } = {},
  p?: CallParams,
): Promise<InboxView> {
  const res = await callUnary(
    t,
    Inbox.method.list,
    { repo: { name: repo }, sections: opts.sections ?? [], pageToken: opts.pageToken ?? "" },
    p,
  );
  return {
    total: res.total,
    next: res.nextPageToken,
    items: res.items.map((i) => ({
      change: changeIdToString(i.change),
      section: i.section,
      latestVersion: i.latestVersion,
      author: i.author,
      updated: Number(i.updated),
      openThreads: i.openThreads,
      pinned: i.pinned,
      requestedBy: i.requestedBy,
    })),
  };
}

export async function countInbox(
  t: BridgeTransport,
  repo: string,
  ifNoneMatch = "",
  p?: CallParams,
): Promise<InboxCounts> {
  const res = await callUnary(t, Inbox.method.count, { repo: { name: repo }, ifNoneMatch }, p);
  return {
    total: res.total,
    etag: res.etag,
    notModified: res.notModified,
    bySection: new Map(res.counts.map((c) => [c.section, c.count])),
  };
}
