// Wire to model for the commit graph: the proto types stop here.
import { toHex } from "@asc/asc-client";
import type { CommitGraphResponse } from "@asc/asc-client/repo";
import type { GraphPage, GraphRow } from "./model";

const decoder = new TextDecoder("utf-8", { fatal: false });

/** First line of the commit message: the row subject. */
function subjectOf(message: Uint8Array): string {
  const text = decoder.decode(message);
  const nl = text.indexOf("\n");
  return nl < 0 ? text : text.slice(0, nl);
}

export function graphPageFromProto(res: CommitGraphResponse): GraphPage {
  const rows = res.rows.map((r): GraphRow => {
    const e = r.entry;
    const review = r.review
      ? { number: Number(r.review.number), state: r.review.state, draft: r.review.draft }
      : undefined;
    return {
      id: e ? toHex(e.id) : `wip:${r.wsId}`,
      subject: e ? subjectOf(e.message) : "Uncommitted changes",
      author: e?.author ? decoder.decode(e.author.name) : "",
      time: e?.author ? Number(e.author.time) * 1000 : 0,
      parents: e ? e.parents.map((p) => toHex(p)) : [],
      column: r.column,
      edges: r.edges.map((x) => ({ toColumn: x.toColumn, parent: toHex(x.parent) })),
      refs: r.refs,
      overflow: r.overflow,
      wip: r.wip,
      ...(review === undefined ? {} : { review }),
    };
  });
  // `total` and `ref_markers` exist only on the first page and may be omitted by the server (A2).
  const hasTotal = res.total > 0n;
  return {
    rows,
    ...(res.nextPageToken === "" ? {} : { next: res.nextPageToken }),
    ...(hasTotal ? { total: Number(res.total) } : {}),
    ...(hasTotal && res.refMarkers.length > 0
      ? { refMarkers: res.refMarkers.map((m) => ({ name: m.name, row: Number(m.row) })) }
      : {}),
  };
}
