// Pure preview appliers over the `Stacks` model (IP-D10). Each returns the predicted model or the
// reason the mutation cannot apply. Nothing here mutates its input or touches the host.
import {
  isDescendantOrSelf,
  type Located,
  locate,
  type Stack,
  type StackChange,
  type Stacks,
} from "../state/model";
import type { Mutation } from "./mutations";

export type PreviewResult =
  | { readonly ok: true; readonly model: Stacks }
  | { readonly ok: false; readonly reason: string };

const fail = (reason: string): PreviewResult => ({ ok: false, reason });
const done = (model: Stacks): PreviewResult => ({ ok: true, model });

export function applyPreview(model: Stacks, m: Mutation): PreviewResult {
  switch (m.kind) {
    case "move":
      return applyMove(model, m);
    case "reorder":
      return applyReorder(model, m);
    case "drop":
      return applyRemove(model, m.change, false);
    case "fold":
      return applyRemove(model, m.change, true);
    case "reword":
      return applyReword(model, m);
    case "restack":
      return applyRestack(model, m.change);
  }
}

function replaceStack(model: Stacks, at: number, next: Stack | undefined): Stacks {
  const stacks = model.stacks.flatMap((s, i) => (i === at ? (next ? [next] : []) : [s]));
  return { ...model, stacks };
}

function setParent(c: StackChange, parent: string | undefined): StackChange {
  if (parent !== undefined) return { ...c, parent };
  const { parent: _parent, ...rest } = c;
  return rest;
}

function applyMove(model: Stacks, m: Extract<Mutation, { kind: "move" }>): PreviewResult {
  const at = locate(model, m.change);
  if (!at) return fail(`Change ${m.change} is no longer in a stack`);
  const stack = model.stacks[at.stackIndex];
  if (!stack) return fail("Stack is gone");
  const moved = stack.changes.slice(at.index);
  if (moved.some((c) => c.frozen)) return fail("A frozen change cannot be moved");

  let destTrunk: string;
  let destBase: string;
  let destParent: string | undefined;
  let destTarget: Located | undefined;
  if (m.onto.kind === "trunk") {
    const name = m.onto.name;
    const trunk = model.trunks.find((t) => t.name === name);
    if (!trunk) return fail(`Unknown trunk ${name}`);
    destTrunk = trunk.name;
    destBase = trunk.tip;
  } else {
    const target = locate(model, m.onto.id);
    if (!target) return fail(`Target ${m.onto.id} is no longer in a stack`);
    if (isDescendantOrSelf(model, m.onto.id, m.change)) {
      return fail("A change cannot move onto itself or its descendants");
    }
    const ts = model.stacks[target.stackIndex];
    if (!ts) return fail("Target stack is gone");
    destTrunk = ts.trunk;
    destBase = ts.base;
    destParent = target.change.id;
    destTarget = target;
  }

  const rebased = moved.map((c, i) =>
    i === 0 ? { ...setParent(c, destParent), needsRestack: false } : { ...c, needsRestack: false },
  );
  const remaining = stack.changes.slice(0, at.index);
  const next = replaceStack(
    model,
    at.stackIndex,
    remaining.length ? { ...stack, changes: remaining } : undefined,
  );

  if (destTarget) {
    const t = locate(next, destTarget.change.id);
    const ts = t && next.stacks[t.stackIndex];
    // Onto a tip the changes extend that stack; onto a middle change they fork a new stack.
    if (t && ts && t.index === ts.changes.length - 1) {
      return done(
        replaceStack(next, t.stackIndex, { ...ts, changes: [...ts.changes, ...rebased] }),
      );
    }
  }
  const fresh: Stack = { trunk: destTrunk, base: destBase, behind: 0, changes: rebased };
  return done({ ...next, stacks: [...next.stacks, fresh] });
}

function applyReorder(model: Stacks, m: Extract<Mutation, { kind: "reorder" }>): PreviewResult {
  const stack = model.stacks[m.stackIndex];
  if (!stack) return fail("Stack is gone");
  const ids = new Set(m.order);
  if (
    m.order.length !== stack.changes.length ||
    ids.size !== m.order.length ||
    !stack.changes.every((c) => ids.has(c.id))
  ) {
    return fail("The stack changed; the new order no longer matches its changes");
  }
  if (stack.changes.some((c) => c.frozen)) return fail("A frozen change cannot be reordered");
  const byId = new Map(stack.changes.map((c) => [c.id, c]));
  let prev: string | undefined = stack.changes[0]?.parent;
  const changes: StackChange[] = [];
  for (const id of m.order) {
    const c = byId.get(id);
    if (!c) return fail("The stack changed");
    changes.push({ ...setParent(c, prev), needsRestack: true });
    prev = id;
  }
  return done(replaceStack(model, m.stackIndex, { ...stack, changes }));
}

/** `drop` abandons the change; `fold` melds it into its parent. Both reparent the children. */
function applyRemove(model: Stacks, id: string, fold: boolean): PreviewResult {
  const at = locate(model, id);
  if (!at) return fail(`Change ${id} is no longer in a stack`);
  if (at.change.frozen) return fail("A frozen change cannot be changed");
  const parent = at.change.parent;
  if (fold && parent === undefined) return fail("A change on the trunk has no parent to fold into");

  if (fold && parent !== undefined) {
    const p = locate(model, parent);
    if (!p) return fail("The parent is no longer in a stack");
    if (p.change.frozen) return fail("A frozen parent cannot be folded into");
  }
  const stacks: Stack[] = [];
  for (const s of model.stacks) {
    const changes = s.changes
      .filter((c) => c.id !== id)
      .map((c) => {
        if (c.parent === id) return { ...setParent(c, parent), needsRestack: true };
        if (fold && c.id === parent) return { ...c, empty: c.empty && at.change.empty };
        return c;
      });
    if (changes.length) stacks.push({ ...s, changes });
  }
  return done({ ...model, stacks });
}

function applyReword(model: Stacks, m: Extract<Mutation, { kind: "reword" }>): PreviewResult {
  const at = locate(model, m.change);
  if (!at) return fail(`Change ${m.change} is no longer in a stack`);
  if (at.change.frozen) return fail("A frozen change cannot be reworded");
  if (m.subject.trim() === "") return fail("The subject is empty");
  const stack = model.stacks[at.stackIndex];
  if (!stack) return fail("Stack is gone");
  const changes = stack.changes.map((c, i) => (i === at.index ? { ...c, subject: m.subject } : c));
  return done(replaceStack(model, at.stackIndex, { ...stack, changes }));
}

/** Clears `needsRestack` on the change and everything above it in its stack. */
function applyRestack(model: Stacks, id: string): PreviewResult {
  const at = locate(model, id);
  if (!at) return fail(`Change ${id} is no longer in a stack`);
  const stack = model.stacks[at.stackIndex];
  if (!stack) return fail("Stack is gone");
  const changes = stack.changes.map((c, i) => (i >= at.index ? { ...c, needsRestack: false } : c));
  return done(replaceStack(model, at.stackIndex, { ...stack, changes }));
}
