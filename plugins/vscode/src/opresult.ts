// The typed result of every `Bridge.Run` verb (`cli.proto` `OpResult`, SPEC 20.7). The bridge sends it as
// the `op_result` event: an encoded message, never text. Everything in it is data from a child process, so
// the decode is bounded and a miss gives null; callers fail closed on null.
import { changeIdToString, shortChangeId } from "@asc/asc-client";
import {
  type OpResult,
  OpResultSchema,
  OpStatus,
  type StashEntry as WireStash,
  SkipReason,
} from "@asc/asc-client/cli";
import { fromBinary } from "@bufbuild/protobuf";

/** The bridge caps a result line at 8 MiB; a plugin reads far less than that. */
export const MAX_OP_RESULT_BYTES = 4 * 1024 * 1024;

/** Decodes the `op_result` bytes; null for absent, oversized or undecodable input. */
export function decodeOpResult(bytes: Uint8Array | null): OpResult | null {
  if (bytes === null || bytes.byteLength === 0 || bytes.byteLength > MAX_OP_RESULT_BYTES) {
    return null;
  }
  try {
    return fromBinary(OpResultSchema, bytes);
  } catch {
    return null;
  }
}

export type OpStatusName = "ok" | "nothing" | "paused" | "ambiguous" | "unknown";

export function statusName(s: OpStatus): OpStatusName {
  switch (s) {
    case OpStatus.OK:
      return "ok";
    case OpStatus.NOTHING:
      return "nothing";
    case OpStatus.PAUSED:
      return "paused";
    case OpStatus.AMBIGUOUS:
      return "ambiguous";
    default:
      return "unknown";
  }
}

const SKIP_TEXT: Record<number, string> = {
  [SkipReason.CHECKED_OUT_ELSEWHERE]: "checked out in another worktree",
  [SkipReason.FROZEN]: "frozen",
  [SkipReason.CONFLICT]: "has a conflict",
  [SkipReason.MERGE]: "is a merge",
  [SkipReason.IMMUTABLE]: "is immutable",
};

const utf8 = new TextDecoder("utf-8", { fatal: false });

/** A change id as its short letters; a malformed id reads as "change". */
export function changeLabel(id: Uint8Array): string {
  try {
    return shortChangeId(changeIdToString(id));
  } catch {
    return "change";
  }
}

/** A change id as the full 32 letters, or null when the bytes are not a change id. */
export function changeFull(id: Uint8Array): string | null {
  try {
    return changeIdToString(id);
  } catch {
    return null;
  }
}

export interface OpLine {
  label: string;
  detail?: string;
}

export interface OpSummary {
  status: OpStatusName;
  lines: OpLine[];
}

/** Lines a person reviews: rewritten changes, skipped, conflicts and hints. Text is clipped. */
export function summariseOp(r: OpResult): OpSummary {
  const lines: OpLine[] = [];
  for (const x of r.rewritten) lines.push({ label: changeLabel(x.change) });
  for (const x of r.skipped) {
    lines.push({
      label: `${changeLabel(x.change)} (skipped)`,
      detail: SKIP_TEXT[x.reason] ?? "skipped",
    });
  }
  for (const x of r.conflicts) {
    lines.push({
      label: `${changeLabel(x.change)} (conflict)`,
      detail: x.paths.map((p) => utf8.decode(p)).join(", "),
    });
  }
  for (const h of r.hints) lines.push({ label: h });
  return { status: statusName(r.status), lines };
}

export interface StashRow {
  index: number;
  message: string;
}

/** The entries of a `stash list` result; anything else gives []. */
export function stashEntries(r: OpResult | null): StashRow[] {
  if (r === null || r.detail.case !== "stash") return [];
  return r.detail.value.entries.map((e: WireStash) => ({
    index: e.index,
    message: utf8.decode(e.message),
  }));
}
