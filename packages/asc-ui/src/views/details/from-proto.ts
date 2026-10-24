// Wire to model for commit details: the proto types stop here. `ReadChangeset` gives the header
// and path changes, `ResolveChanges` the change and review, `Versions` the version list and
// `Log` with `with_stats` the per-file counts.
import { toHex } from "@asc/asc-client";
import type { ChangeInfo, Version } from "@asc/asc-client/changes";
import type { FileStat, PathChange, ReadChangesetResponse, Signature } from "@asc/asc-client/repo";
import { type DetailFile, type DetailPerson, type DetailsData, statusOf } from "./model";

const decoder = new TextDecoder("utf-8", { fatal: false });

function person(s: Signature | undefined): DetailPerson {
  return s
    ? { name: decoder.decode(s.name), email: decoder.decode(s.email), time: Number(s.time) }
    : { name: "", email: "", time: 0 };
}

export function filesFromProto(changes: readonly PathChange[], stats: readonly FileStat[] = []): DetailFile[] {
  const byPath = new Map(stats.map((s) => [decoder.decode(s.path), s]));
  return changes.map((c): DetailFile => {
    const path = decoder.decode(c.path);
    const s = byPath.get(path);
    return {
      path,
      status: statusOf(c.status),
      ...(c.oldPath.length > 0 ? { oldPath: decoder.decode(c.oldPath) } : {}),
      ...(s ? { added: s.added, removed: s.removed, binary: s.binary } : {}),
    };
  });
}

export interface DetailsInput {
  readonly changeset: ReadChangesetResponse;
  readonly info?: ChangeInfo;
  readonly versions?: readonly Version[];
  readonly stats?: readonly FileStat[];
  readonly signature?: string;
}

export function detailsFromProto(input: DetailsInput): DetailsData {
  const h = input.changeset.header;
  const r = input.info?.review;
  return {
    id: h ? toHex(h.id) : "",
    parents: h ? h.parents.map((p) => toHex(p)) : [],
    message: h ? decoder.decode(h.message) : "",
    author: person(h?.author),
    ...(h?.committer ? { committer: person(h.committer) } : {}),
    ...(input.signature === undefined ? {} : { signature: input.signature }),
    ...(h && h.change.length > 0 ? { change: toHex(h.change) } : {}),
    ...(r && r.number > 0n
      ? {
          review: {
            forge: r.forge,
            number: Number(r.number),
            url: r.url,
            state: r.state,
            draft: r.draft,
          },
        }
      : {}),
    versions: (input.versions ?? []).map((v) => ({
      v: v.v,
      rev: toHex(v.cs),
      at: Number(v.at),
      actor: v.actor,
    })),
    files: filesFromProto(input.changeset.changes, input.stats),
  };
}
