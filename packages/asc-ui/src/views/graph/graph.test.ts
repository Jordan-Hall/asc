import { describe, expect, test } from "bun:test";
import { genRow } from "./fixture";
import {
  type GraphPage,
  type GraphRow,
  laneCount,
  MAX_ROWS,
  menuCall,
  menuItems,
  minimapModel,
  minimapRow,
  navigate,
  PAGE_SIZE,
  PageStore,
  parseRefList,
  REASON_MERGE,
  REASON_WIP,
  searchRow,
  visibleRange,
} from "./model";

const TOTAL = 100_000;
const page = (p: number, withHead = false): GraphPage => {
  const rows: GraphRow[] = [];
  for (let i = p * PAGE_SIZE; i < Math.min(TOTAL, (p + 1) * PAGE_SIZE); i++)
    rows.push(genRow(i, TOTAL));
  return {
    rows,
    ...((p + 1) * PAGE_SIZE < TOTAL ? { next: `t${p + 1}` } : {}),
    ...(withHead
      ? {
          total: TOTAL,
          refMarkers: [
            { name: "main", row: 0 },
            { name: "tag-50", row: 50 },
          ],
        }
      : {}),
  };
};

describe("PageStore", () => {
  test("never holds more than 5,000 rows, evicting the pages farthest from the focus", () => {
    const s = new PageStore();
    for (let p = 0; p < 60; p++) {
      s.setFocus(p * PAGE_SIZE);
      s.add(p, page(p, p === 0));
      expect(s.heldRows).toBeLessThanOrEqual(MAX_ROWS);
    }
    expect(s.heldRows).toBe(MAX_ROWS);
    expect(s.hasPage(59)).toBe(true);
    expect(s.hasPage(0)).toBe(false);
    expect(s.tokenFor(60)).toBe("t60");
    expect(s.total).toBe(TOTAL);
  });

  test("rowAt and indexOf agree and rows carry pass-through lanes", () => {
    const s = new PageStore();
    s.add(0, page(0, true));
    const m = s.rowAt(0);
    expect(m?.id).toBe(genRow(0, TOTAL).id);
    expect(s.indexOf(genRow(7, TOTAL).id)).toBe(7);
    // row 0 (merge) opens lanes 0 and 1; row 1 (side, col 1) ends lane 1 and lane 0 passes
    expect(s.rowAt(1)?.pass).toEqual([0]);
    expect(s.rowAt(1)?.incoming).toEqual([1]);
    // the fork row 4 ends both lanes
    expect(s.rowAt(4)?.incoming?.toSorted()).toEqual([0, 1]);
  });

  test("rowCount is exact with a total, else loaded pages plus one", () => {
    const s = new PageStore();
    s.add(0, page(0));
    expect(s.total).toBeUndefined();
    expect(s.rowCount).toBe(2 * PAGE_SIZE);
    const t = new PageStore();
    t.add(0, page(0, true));
    expect(t.rowCount).toBe(TOTAL);
  });

  test("the last page ends the chain", () => {
    const s = new PageStore();
    const last = Math.ceil(TOTAL / PAGE_SIZE) - 1;
    s.add(last, page(last));
    expect(s.isBeyondEnd(last + 1)).toBe(true);
    expect(s.isBeyondEnd(last)).toBe(false);
  });

  test("spliceHead keeps later pages when nothing moved, and drops them when rows were added", () => {
    const s = new PageStore();
    s.add(0, page(0, true));
    s.add(1, page(1));
    expect(s.spliceHead(page(0, true))).toBe("spliced");
    expect(s.hasPage(1)).toBe(true);
    const moved: GraphPage = {
      ...page(0, true),
      rows: [genRow(900_000, TOTAL), ...page(0).rows.slice(0, PAGE_SIZE - 1)],
    };
    expect(s.spliceHead(moved)).toBe("reset");
    expect(s.hasPage(1)).toBe(false);
    expect(s.hasPage(0)).toBe(true);
  });
});

describe("minimap", () => {
  test("uses the first page total and ref markers", () => {
    const s = new PageStore();
    s.add(0, page(0, true));
    const m = minimapModel(s);
    expect(m.exact).toBe(true);
    expect(m.span).toBe(TOTAL);
    expect(m.ticks.map((t) => t.name)).toEqual(["main", "tag-50"]);
    expect(m.loaded).toEqual([{ from: 0, to: PAGE_SIZE / TOTAL }]);
  });

  test("without total it spans only the loaded pages and has no ticks", () => {
    const s = new PageStore();
    s.add(0, page(0));
    s.add(1, page(1));
    const m = minimapModel(s);
    expect(m.exact).toBe(false);
    expect(m.span).toBe(2 * PAGE_SIZE);
    expect(m.ticks).toEqual([]);
  });

  test("a click maps to a row inside the span", () => {
    expect(minimapRow(0.5, 1000)).toBe(500);
    expect(minimapRow(1.5, 1000)).toBe(999);
    expect(minimapRow(-1, 1000)).toBe(0);
  });
});

describe("navigation and search", () => {
  const s = new PageStore();
  s.add(0, page(0, true));
  test("parent and child follow the commit links", () => {
    // row 3 (main1) has parent F = row 4
    expect(navigate(s, 3, "parent", TOTAL)).toBe(4);
    expect(navigate(s, 4, "child", TOTAL)).toBe(3);
  });
  test("next fork and next ref", () => {
    expect(navigate(s, 1, "nextFork", TOTAL)).toBe(25);
    expect(navigate(s, 0, "nextRef", TOTAL)).toBe(50);
    expect(navigate(s, 60, "prevRef", TOTAL)).toBe(50);
  });
  test("down and up clamp to the list", () => {
    expect(navigate(s, 0, "up", TOTAL)).toBe(0);
    expect(navigate(s, TOTAL - 1, "down", TOTAL)).toBe(TOTAL - 1);
    expect(navigate(s, 0, "last", TOTAL)).toBe(TOTAL - 1);
  });
  test("a scan stops at the first unloaded row so the view can load it", () => {
    expect(navigate(s, 150, "nextRef", TOTAL)).toBe(200);
  });
  test("search wraps over loaded rows and matches refs and authors", () => {
    expect(searchRow(s, 0, "commit 12")).toBe(12);
    expect(searchRow(s, 100, "tag-50")).toBe(50);
    expect(searchRow(s, 0, "nothing-like-this")).toBeUndefined();
    expect(searchRow(s, 0, "  ")).toBeUndefined();
  });
});

describe("virtual window", () => {
  test("overscan 20 on both sides, clamped", () => {
    expect(visibleRange(0, 220, 22, 1000)).toEqual({ first: 0, end: 30 });
    expect(visibleRange(2200, 220, 22, 1000)).toEqual({ first: 80, end: 130 });
    expect(visibleRange(21_900, 220, 22, 1000)).toEqual({ first: 975, end: 1000 });
  });
  test("lane count is clamped", () => {
    const wide = { ...genRow(0, 10), column: 99 };
    expect(laneCount([wide, undefined])).toBe(24);
    expect(laneCount([genRow(0, 100)])).toBe(2);
  });
});

describe("menu", () => {
  test("cherry-pick and revert are enabled on a normal commit", () => {
    const items = menuItems(genRow(10, TOTAL));
    expect(items.map((i) => [i.id, i.enabled])).toEqual([
      ["cherry-pick", true],
      ["revert", true],
      ["copy-id", true],
    ]);
  });
  test("a merge states why it is disabled", () => {
    const [cp] = menuItems(genRow(0, TOTAL));
    expect(cp).toMatchObject({ enabled: false, reason: REASON_MERGE });
  });
  test("a wip row states why it is disabled", () => {
    const wip = { ...genRow(10, TOTAL), wip: true };
    for (const i of menuItems(wip)) expect(i).toMatchObject({ enabled: false, reason: REASON_WIP });
  });
  test("the verb call carries the full id only", () => {
    const row = genRow(10, TOTAL);
    expect(menuCall("revert", row)).toEqual({ verb: "revert", rev: row.id });
  });
  test("filter lists split on commas and spaces", () => {
    expect(parseRefList(" a, b  c ,, ")).toEqual(["a", "b", "c"]);
  });
});
