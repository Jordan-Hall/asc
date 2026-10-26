// Builders for the encoded `OpResult` the bridge sends as `op_result`, so tests exercise the real decode.
import { changeIdToString } from "@asc/asc-client";
import { OpResultSchema, OpStatus } from "@asc/asc-client/cli";
import { create, toBinary } from "@bufbuild/protobuf";

const enc = new TextEncoder();

/** A 16-byte change id whose first byte is `n`. */
export function cid(n: number): Uint8Array {
  const b = new Uint8Array(16);
  b[0] = n;
  return b;
}

/** The 32-letter text of `cid(n)`. */
export const cidText = (n: number): string => changeIdToString(cid(n));

export interface OpInit {
  status?: "ok" | "nothing" | "paused" | "ambiguous";
  rewritten?: number[];
  candidates?: { n: number; subject: string }[];
  conflicts?: { n: number; paths: string[] }[];
  hints?: string[];
  stash?: { index: number; message: string }[];
}

const STATUS = {
  ok: OpStatus.OK,
  nothing: OpStatus.NOTHING,
  paused: OpStatus.PAUSED,
  ambiguous: OpStatus.AMBIGUOUS,
} as const;

export function opBytes(i: OpInit = {}): Uint8Array {
  return toBinary(
    OpResultSchema,
    create(OpResultSchema, {
      version: 1,
      status: STATUS[i.status ?? "ok"],
      rewritten: (i.rewritten ?? []).map((n) => ({
        change: cid(n),
        old: new Uint8Array(20),
        new: new Uint8Array(20),
      })),
      candidates: (i.candidates ?? []).map((c) => ({
        change: cid(c.n),
        subject: enc.encode(c.subject),
      })),
      conflicts: (i.conflicts ?? []).map((c) => ({
        change: cid(c.n),
        paths: c.paths.map((p) => enc.encode(p)),
      })),
      hints: i.hints ?? [],
      detail:
        i.stash === undefined
          ? { case: undefined }
          : {
              case: "stash",
              value: {
                action: "list",
                entries: i.stash.map((e) => ({
                  index: e.index,
                  commit: new Uint8Array(20),
                  message: enc.encode(e.message),
                })),
              },
            },
    }),
  );
}
