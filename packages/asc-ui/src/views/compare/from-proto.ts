// Wire to model for compare: the proto types stop here.
import { toHex } from "@asc/asc-client";
import type { CompareResponse, LogEntry } from "@asc/asc-client/repo";
import { filesFromProto } from "../details/from-proto";
import type { CompareData, CompareEntry, CompareSide } from "./model";

const decoder = new TextDecoder("utf-8", { fatal: false });

function entry(e: LogEntry): CompareEntry {
  const text = decoder.decode(e.message);
  const nl = text.indexOf("\n");
  return {
    id: toHex(e.id),
    subject: nl < 0 ? text : text.slice(0, nl),
    author: e.author ? decoder.decode(e.author.name) : "",
    time: e.author ? Number(e.author.time) : 0,
  };
}

export function compareFromProto(res: CompareResponse, a: CompareSide, b: CompareSide): CompareData {
  return {
    a,
    b,
    mergeBase: toHex(res.mergeBase),
    ahead: Number(res.ahead),
    behind: Number(res.behind),
    aheadEntries: res.aheadEntries.map(entry),
    behindEntries: res.behindEntries.map(entry),
    files: filesFromProto(res.files),
    aheadMore: res.aheadNext !== "",
    behindMore: res.behindNext !== "",
  };
}
