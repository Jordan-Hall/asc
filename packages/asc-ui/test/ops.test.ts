import { describe, expect, test } from "bun:test";
import { createStore } from "jotai/vanilla";
import {
  applyPreview,
  bindOpsQueue,
  MAX_FINISHED,
  MAX_QUEUED,
  type Mutation,
  OpsQueue,
  type OpRunner,
  predictedStacksAtom,
  QUEUE_FULL_MESSAGE,
  queueSnapshotAtom,
  type Stack,
  type StackChange,
  type Stacks,
} from "../src";

function change(id: string, parent?: string, extra: Partial<StackChange> = {}): StackChange {
  return {
    id,
    ...(parent === undefined ? {} : { parent }),
    subject: `subject ${id}`,
    branches: [],
    state: "draft",
    needsRestack: false,
    frozen: false,
    empty: false,
    current: false,
    ...extra,
  };
}

const model: Stacks = {
  version: 1,
  trunks: [{ name: "master", tip: "aa", remote: "origin" }],
  stacks: [
    {
      trunk: "master",
      base: "aa",
      behind: 0,
      changes: [change("a"), change("b", "a"), change("c", "b")],
    },
    { trunk: "master", base: "aa", behind: 0, changes: [change("x")] },
  ],
};

const S0 = model.stacks[0] as Stack;
const ids = (m: Stacks, i: number) => m.stacks[i]?.changes.map((c) => c.id);
const ok = (m: Stacks, mut: Mutation) => {
  const r = applyPreview(m, mut);
  if (!r.ok) throw new Error(r.reason);
  return r.model;
};

const tick = () => new Promise((r) => setTimeout(r, 0));

describe("preview appliers", () => {
  test("move onto the tip of another stack appends and moves descendants", () => {
    const m = ok(model, { kind: "move", change: "b", onto: { kind: "change", id: "x" } });
    expect(ids(m, 0)).toEqual(["a"]);
    expect(ids(m, 1)).toEqual(["x", "b", "c"]);
    expect(m.stacks[1]?.changes[1]?.parent).toBe("x");
    expect(model.stacks[0]?.changes.length).toBe(3);
  });

  test("move onto a middle change forks a new stack; onto trunk starts one", () => {
    const forked = ok(model, { kind: "move", change: "x", onto: { kind: "change", id: "a" } });
    expect(forked.stacks.length).toBe(2);
    expect(forked.stacks[1]?.changes[0]?.parent).toBe("a");
    const rooted = ok(model, {
      kind: "move",
      change: "b",
      onto: { kind: "trunk", name: "master" },
    });
    expect(rooted.stacks[2]?.changes[0]?.parent).toBeUndefined();
  });

  test("move onto own descendant, unknown trunk and frozen are refused", () => {
    expect(
      applyPreview(model, { kind: "move", change: "a", onto: { kind: "change", id: "c" } }).ok,
    ).toBe(false);
    expect(
      applyPreview(model, { kind: "move", change: "a", onto: { kind: "trunk", name: "nope" } }).ok,
    ).toBe(false);
    const frozen: Stacks = {
      ...model,
      stacks: [{ ...S0, changes: [change("a", undefined, { frozen: true })] }],
    };
    expect(
      applyPreview(frozen, { kind: "move", change: "a", onto: { kind: "trunk", name: "master" } })
        .ok,
    ).toBe(false);
  });

  test("reorder rewrites parents and rejects a stale order", () => {
    const m = ok(model, { kind: "reorder", stackIndex: 0, order: ["c", "a", "b"] });
    expect(m.stacks[0]?.changes.map((c) => [c.id, c.parent])).toEqual([
      ["c", undefined],
      ["a", "c"],
      ["b", "a"],
    ]);
    expect(applyPreview(model, { kind: "reorder", stackIndex: 0, order: ["a", "b"] }).ok).toBe(
      false,
    );
  });

  test("drop and fold reparent children; the last change removes its stack", () => {
    expect(ids(ok(model, { kind: "drop", change: "b" }), 0)).toEqual(["a", "c"]);
    expect(ok(model, { kind: "drop", change: "b" }).stacks[0]?.changes[1]?.parent).toBe("a");
    expect(ids(ok(model, { kind: "fold", change: "b" }), 0)).toEqual(["a", "c"]);
    expect(applyPreview(model, { kind: "fold", change: "a" }).ok).toBe(false);
    expect(ok(model, { kind: "drop", change: "x" }).stacks.length).toBe(1);
  });

  test("reword and restack", () => {
    expect(
      ok(model, { kind: "reword", change: "a", subject: "new" }).stacks[0]?.changes[0]?.subject,
    ).toBe("new");
    expect(applyPreview(model, { kind: "reword", change: "a", subject: " " }).ok).toBe(false);
    const stale: Stacks = {
      ...model,
      stacks: [{ ...S0, changes: S0.changes.map((c) => ({ ...c, needsRestack: true })) }],
    };
    expect(
      ok(stale, { kind: "restack", change: "b" }).stacks[0]?.changes.map((c) => c.needsRestack),
    ).toEqual([true, false, false]);
  });
});

function gated() {
  const gates: Array<(o: { ok: true } | { ok: false; error: string }) => void> = [];
  const calls: Mutation[] = [];
  const run: OpRunner = (m) => {
    calls.push(m);
    return new Promise((resolve) => gates.push(resolve));
  };
  return { run, gates, calls };
}

describe("ops queue", () => {
  test("runs one at a time in order and keeps the preview until Watch confirms", async () => {
    const g = gated();
    const q = new OpsQueue({ root: "r", run: g.run, confirmed: model });
    q.enqueue({ kind: "reword", change: "a", subject: "one" });
    q.enqueue({ kind: "reword", change: "b", subject: "two" });
    await tick();
    expect(g.calls.length).toBe(1);
    expect(q.snapshot().running?.status).toBe("running");
    expect(q.snapshot().queued.length).toBe(1);
    expect(q.predicted().stacks[0]?.changes[1]?.subject).toBe("two");

    g.gates[0]?.({ ok: true });
    await tick();
    expect(g.calls.length).toBe(2);
    expect(q.snapshot().optimistic.length).toBe(1);
    expect(q.predicted().stacks[0]?.changes[0]?.subject).toBe("one");

    g.gates[1]?.({ ok: true });
    await tick();
    expect(q.snapshot().optimistic.length).toBe(2);
    q.confirm(
      ok(ok(model, { kind: "reword", change: "a", subject: "one" }), {
        kind: "reword",
        change: "b",
        subject: "two",
      }),
    );
    expect(q.snapshot().optimistic.length).toBe(0);
    expect(q.snapshot().finished.map((o) => o.status)).toEqual(["done", "done"]);
  });

  test("a failed op drops its preview", async () => {
    const g = gated();
    const q = new OpsQueue({ root: "r", run: g.run, confirmed: model });
    q.enqueue({ kind: "drop", change: "x" });
    await tick();
    g.gates[0]?.({ ok: false, error: "boom" });
    await tick();
    expect(q.predicted().stacks.length).toBe(2);
    expect(q.snapshot().finished[0]).toMatchObject({ status: "failed", reason: "boom" });
  });

  test("a queued op whose preview no longer applies after a Watch event is cancelled", async () => {
    const g = gated();
    const q = new OpsQueue({ root: "r", run: g.run, confirmed: model });
    q.enqueue({ kind: "reword", change: "a", subject: "one" });
    q.enqueue({ kind: "drop", change: "x" });
    q.enqueue({ kind: "reword", change: "b", subject: "keep" });
    await tick();
    // The server model no longer has change x.
    q.confirm({ ...model, stacks: [S0] });
    const snap = q.snapshot();
    expect(snap.running?.mutation).toMatchObject({ change: "a" });
    expect(snap.queued.map((o) => o.mutation)).toEqual([
      { kind: "reword", change: "b", subject: "keep" },
    ]);
    expect(snap.finished[0]).toMatchObject({ status: "cancelled" });
    expect(snap.finished[0]?.reason).toContain("stack changed");
  });

  test("caps: 50 queued, refusal message, 20 finished kept", async () => {
    const g = gated();
    const q = new OpsQueue({ root: "r", run: g.run, confirmed: model });
    // First op starts running and leaves the queue, so 51 are accepted: 1 running + 50 queued.
    for (let i = 0; i < MAX_QUEUED + 1; i++) {
      const r = q.enqueue({ kind: "reword", change: "a", subject: `s${i}` });
      expect(r.ok).toBe(true);
    }
    const refused = q.enqueue({ kind: "reword", change: "a", subject: "over" });
    expect(refused).toEqual({ ok: false, reason: QUEUE_FULL_MESSAGE });

    for (let i = 0; i < 30; i++) {
      q.cancel(q.snapshot().queued[0]?.id ?? -1);
    }
    expect(q.snapshot().finished.length).toBe(MAX_FINISHED);
    expect(q.snapshot().queued.length).toBe(MAX_QUEUED - 30);
  });

  test("enqueue refuses a mutation that does not apply to the predicted model", () => {
    const q = new OpsQueue({ root: "r", run: async () => ({ ok: true }), confirmed: model });
    expect(q.enqueue({ kind: "drop", change: "missing" }).ok).toBe(false);
  });

  test("a throwing runner fails the op without stalling the queue", async () => {
    let n = 0;
    const q = new OpsQueue({
      root: "r",
      confirmed: model,
      run: async () => {
        if (n++ === 0) throw new Error("bridge down");
        return { ok: true };
      },
    });
    q.enqueue({ kind: "restack", change: "a" });
    q.enqueue({ kind: "restack", change: "b" });
    await tick();
    await tick();
    expect(q.snapshot().finished.some((o) => o.reason === "bridge down")).toBe(true);
    expect(q.snapshot().optimistic.length).toBe(1);
  });
});

describe("jotai binding", () => {
  test("atoms follow the queue", async () => {
    const store = createStore();
    const c = bindOpsQueue(store, "r", async () => ({ ok: true }));
    c.confirm(model);
    c.queue.enqueue({ kind: "drop", change: "x" });
    await tick();
    expect(store.get(predictedStacksAtom).stacks.length).toBe(1);
    expect(store.get(queueSnapshotAtom).optimistic.length).toBe(1);
    c.confirm({ ...model, stacks: [S0] });
    expect(store.get(queueSnapshotAtom).optimistic.length).toBe(0);
  });
});
