import { describe, expect, test } from "bun:test";
import { type AffectedRow, filterAffected, sortAffected } from "../affected/model";
import { layoutGraph, neighbourhood } from "../projects/model";
import { rerunDisabledReason, runTone, tailLog } from "./model";

const row = (name: string, direct: boolean): AffectedRow => ({
  id: name,
  name,
  root: name,
  direct,
  reasons: [],
  targets: {},
});

describe("actions", () => {
  test("tone and rerun reason", () => {
    expect(runTone("success")).toBe("ok");
    expect(runTone("waiting")).toBe("pending");
    expect(rerunDisabledReason("running")).toBeDefined();
    expect(rerunDisabledReason("failure")).toBeUndefined();
  });
  test("tailLog strips escapes and keeps the tail", () => {
    const r = tailLog("a\n\u001b[31mb\u001b[0m\r\nc\n", 2);
    expect(r).toEqual({ text: "b\nc", truncated: true });
  });
});

describe("affected", () => {
  test("direct first then by name; filter", () => {
    const s = sortAffected([row("z", true), row("b", false), row("a", false)]);
    expect(s.map((r) => r.name)).toEqual(["z", "a", "b"]);
    expect(filterAffected(s, "B").map((r) => r.name)).toEqual(["b"]);
  });
});

describe("projects", () => {
  const ps = [
    { id: "a", name: "a", deps: [] },
    { id: "b", name: "b", deps: ["a"] },
    { id: "c", name: "c", deps: ["b", "ghost"] },
    { id: "d", name: "d", deps: [] },
  ];
  test("layers and edges", () => {
    const l = layoutGraph(ps);
    expect(l.nodes.find((n) => n.node.id === "c")?.layer).toBe(2);
    expect(l.edges).toHaveLength(2);
    expect(l.cyclic).toEqual([]);
  });
  test("cycles terminate", () => {
    const l = layoutGraph([
      { id: "x", name: "x", deps: ["y"] },
      { id: "y", name: "y", deps: ["x"] },
    ]);
    expect(l.cyclic.length).toBeGreaterThan(0);
  });
  test("neighbourhood", () => {
    expect([...neighbourhood(ps, "b")].toSorted()).toEqual(["a", "b", "c"]);
  });
});
