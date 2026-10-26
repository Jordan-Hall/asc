import { describe, expect, test } from "bun:test";
import {
  appendCapped,
  coverageSummary,
  engineFor,
  flattenFileHits,
  highlightRuns,
  MAX_HITS,
  modeAvailability,
  queryBlockedReason,
  REASON_EMPTY,
  REASON_NO_SEARCH_V1,
  REASON_SCOPE_VALUE,
  resultCountText,
  scopeAvailability,
  type ResultItem,
  type SearchQuery,
} from "./model";

const on = { searchV1: true };
const off = { searchV1: false };
const q = (over: Partial<SearchQuery> = {}): SearchQuery => ({
  mode: "exact",
  text: "x",
  scope: { kind: "workspace", value: "" },
  caseSensitive: false,
  ...over,
});
const item = (n: number): ResultItem => ({
  path: `f${n}`,
  line: n,
  text: "",
  cols: [],
  length: 0,
  redacted: false,
});

describe("feature gating", () => {
  test("modes beyond exact need search.v1", () => {
    expect(modeAvailability("exact", off).enabled).toBe(true);
    for (const m of ["regex", "symbol", "semantic"] as const) {
      expect(modeAvailability(m, off)).toEqual({ enabled: false, reason: REASON_NO_SEARCH_V1 });
      expect(modeAvailability(m, on).enabled).toBe(true);
    }
  });
  test("change and stack scopes need search.v1", () => {
    expect(scopeAvailability("workspace", off).enabled).toBe(true);
    expect(scopeAvailability("revision", off).enabled).toBe(true);
    expect(scopeAvailability("change", off).reason).toBe(REASON_NO_SEARCH_V1);
    expect(scopeAvailability("stack", on).enabled).toBe(true);
  });
  test("engine falls back to grep only for exact without search.v1", () => {
    expect(engineFor(q(), off)).toBe("grep");
    expect(engineFor(q(), on)).toBe("codeintel");
    expect(engineFor(q({ mode: "regex" }), on)).toBe("codeintel");
  });
});

describe("queryBlockedReason", () => {
  test("states the reason", () => {
    expect(queryBlockedReason(q({ text: " " }), on)).toBe(REASON_EMPTY);
    expect(queryBlockedReason(q({ scope: { kind: "change", value: "" } }), on)).toBe(
      REASON_SCOPE_VALUE,
    );
    expect(queryBlockedReason(q({ mode: "semantic" }), off)).toBe(REASON_NO_SEARCH_V1);
    expect(queryBlockedReason(q({ scope: { kind: "stack", value: "a" } }), off)).toBe(
      REASON_NO_SEARCH_V1,
    );
    expect(queryBlockedReason(q(), on)).toBeUndefined();
  });
});

describe("coverage", () => {
  test("summarises indexes and flags partial coverage", () => {
    expect(coverageSummary([{ index: "trigram", coverage: 0.999, lagSeconds: 0 }], on)).toEqual({
      text: "Index: trigram 99%",
      partial: true,
    });
    expect(
      coverageSummary(
        [
          { index: "trigram", coverage: 1, lagSeconds: 0 },
          { index: "vector", coverage: 1, lagSeconds: 4 },
        ],
        on,
      ),
    ).toEqual({ text: "Index: trigram 100% · vector 100%, 4s behind", partial: false });
    expect(coverageSummary([], on).partial).toBe(true);
    expect(coverageSummary(undefined, on).text).toBe("Index status unknown");
    expect(coverageSummary(undefined, off).partial).toBe(false);
  });
});

describe("results", () => {
  test("flatten drops text of redacted files", () => {
    const out = flattenFileHits(
      [
        { path: "a", matches: [{ line: 1, cols: [2], text: "abc" }] },
        { path: "b", redacted: true, matches: [{ line: 2, cols: [0], text: "secret" }] },
      ],
      1,
    );
    expect(out[0]).toMatchObject({ path: "a", cols: [2], text: "abc", redacted: false });
    expect(out[1]).toMatchObject({ path: "b", cols: [], text: "", redacted: true });
  });
  test("highlightRuns clamps and merges overlaps", () => {
    expect(highlightRuns("hello world", [6], 5)).toEqual([
      { text: "hello ", hit: false },
      { text: "world", hit: true },
    ]);
    expect(highlightRuns("abc", [1, 2], 5)).toEqual([
      { text: "a", hit: false },
      { text: "bc", hit: true },
    ]);
    expect(highlightRuns("abc", [99], 2)).toEqual([{ text: "abc", hit: false }]);
    expect(highlightRuns("abc", [0], 0)).toEqual([{ text: "abc", hit: false }]);
  });
  test("appendCapped stops at the cap", () => {
    const base = Array.from({ length: MAX_HITS - 1 }, (_, i) => item(i));
    const r = appendCapped(base, [item(1), item(2)]);
    expect(r.items).toHaveLength(MAX_HITS);
    expect(r.capped).toBe(true);
    expect(appendCapped([], [item(1)]).capped).toBe(false);
  });
  test("count text", () => {
    expect(resultCountText(1, false)).toBe("1 result");
    expect(resultCountText(2, true)).toContain("more available");
  });
});
