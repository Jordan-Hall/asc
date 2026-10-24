import { describe, expect, test } from "bun:test";
import { buildSections, type InboxEntry, needsReviewCount, snoozeUntil } from "./model";

const now = Date.parse("2026-01-01T00:00:00Z");
const pr = (number: number) => ({ number, title: `t${number}` });
const entries: InboxEntry[] = [
  { id: 1, section: "needs_review", pull_request: pr(3) },
  { id: 2, section: "needs_review", pinned: true, pull_request: pr(5) },
  { id: 3, section: "needs_review", snoozed_until: "2026-01-02T00:00:00Z", pull_request: pr(1) },
  { id: 4, section: "needs_review", snoozed_until: "2025-12-31T00:00:00Z", pull_request: pr(2) },
  { id: 5, section: "bogus", pull_request: pr(9) },
];

describe("inbox model", () => {
  test("seven sections in order; unknown dropped", () => {
    const s = buildSections(entries, now);
    expect(s.map((x) => x.id)).toEqual([
      "needs_review",
      "returned",
      "ready",
      "blocked",
      "waiting",
      "drafts",
      "queued",
    ]);
    expect(s.flatMap((x) => x.items).some((i) => i.id === 5)).toBe(false);
  });
  test("pinned first, snoozed hidden, expired snooze shown", () => {
    const s = buildSections(entries, now)[0];
    expect(s?.items.map((i) => i.id)).toEqual([2, 4, 1]);
    expect(s?.snoozedCount).toBe(1);
    expect(buildSections(entries, now, true)[0]?.items.map((i) => i.id)).toEqual([2, 3, 4, 1]);
  });
  test("count and snooze time", () => {
    expect(needsReviewCount(entries, now)).toBe(3);
    expect(snoozeUntil(3_600_000, now)).toBe("2026-01-01T01:00:00.000Z");
  });
});
