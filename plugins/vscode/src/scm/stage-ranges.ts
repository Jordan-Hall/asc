// Stage and unstage line ranges and hunks (SPEC 24.3.1, row "Stage and unstage file, hunk, selected lines").
// The host builds a `git apply --cached` patch from two texts and a line selection; the patch goes to
// `Workspace.Stage { patch }` (the daemon applies it to the index). Pure functions: no editor, no I/O.

/** A 1-based inclusive line range. An empty range (`end < start`) is a point: it selects a deletion
 * that sits at line `start` and nothing else (what a gutter peek reports for a pure deletion). */
export interface LineRange {
  start: number;
  end: number;
}

/** Which side of the diff the ranges number: the `base` (old) text or the `target` (new) text. */
export type Side = "base" | "target";

export type OpKind = "eq" | "del" | "add";
export interface DiffOp {
  kind: OpKind;
  /** The line, including its terminator, so a missing final newline is a difference. */
  text: string;
  /** 1-based line in the base text for `eq` and `del`; for `add`, the next base line. */
  baseNo: number;
  /** 1-based line in the target text for `eq` and `add`; for `del`, the next target line. */
  targetNo: number;
}

/** Beyond this many table cells the middle of the files is treated as one replaced block. */
const MAX_CELLS = 4_000_000;
const NO_EOL = "\u0000no-eol";

/** Splits into lines that keep their terminator; a last line with none is marked so it differs from
 * the same text with one. */
export function splitLines(text: string): string[] {
  if (text === "") return [];
  const out: string[] = [];
  let from = 0;
  for (;;) {
    const i = text.indexOf("\n", from);
    if (i < 0) {
      if (from < text.length) out.push(text.slice(from) + NO_EOL);
      return out;
    }
    out.push(text.slice(from, i + 1));
    from = i + 1;
  }
}

/** A line-level diff with the common prefix and suffix trimmed and an LCS table for the rest. */
export function diffLines(base: string[], target: string[]): DiffOp[] {
  let pre = 0;
  while (pre < base.length && pre < target.length && base[pre] === target[pre]) pre++;
  let suf = 0;
  while (
    suf < base.length - pre &&
    suf < target.length - pre &&
    base[base.length - 1 - suf] === target[target.length - 1 - suf]
  ) {
    suf++;
  }
  const a = base.slice(pre, base.length - suf);
  const b = target.slice(pre, target.length - suf);
  const ops: DiffOp[] = [];
  let bn = 1;
  let tn = 1;
  const push = (kind: OpKind, text: string) => {
    ops.push({ kind, text, baseNo: bn, targetNo: tn });
    if (kind !== "add") bn++;
    if (kind !== "del") tn++;
  };
  for (let i = 0; i < pre; i++) push("eq", base[i] ?? "");
  if ((a.length + 1) * (b.length + 1) > MAX_CELLS) {
    for (const l of a) push("del", l);
    for (const l of b) push("add", l);
  } else {
    const w = b.length + 1;
    const lcs = new Uint32Array((a.length + 1) * w);
    for (let i = a.length - 1; i >= 0; i--) {
      for (let j = b.length - 1; j >= 0; j--) {
        lcs[i * w + j] =
          a[i] === b[j]
            ? (lcs[(i + 1) * w + j + 1] ?? 0) + 1
            : Math.max(lcs[(i + 1) * w + j] ?? 0, lcs[i * w + j + 1] ?? 0);
      }
    }
    let i = 0;
    let j = 0;
    while (i < a.length || j < b.length) {
      if (i < a.length && j < b.length && a[i] === b[j]) {
        push("eq", a[i] ?? "");
        i++;
        j++;
      } else if (
        i < a.length &&
        (j >= b.length || (lcs[(i + 1) * w + j] ?? 0) >= (lcs[i * w + j + 1] ?? 0))
      ) {
        push("del", a[i] ?? "");
        i++;
      } else {
        push("add", b[j] ?? "");
        j++;
      }
    }
  }
  for (let i = base.length - suf; i < base.length; i++) push("eq", base[i] ?? "");
  return ops;
}

/** Whether the op belongs to the selection. An op on the numbered side is selected when its line is
 * in a range. An op on the other side (a deletion when numbering the target) sits between lines, so it
 * is selected when the line it precedes is in a range. */
function selected(op: DiffOp, ranges: readonly LineRange[], points: readonly LineRange[], side: Side) {
  if (op.kind === "eq") return false;
  const own = side === "target" ? op.kind === "add" : op.kind === "del";
  const n = side === "target" ? op.targetNo : op.baseNo;
  if (own) return ranges.some((r) => n >= r.start && n <= r.end);
  return ranges.some((r) => n >= r.start && n <= r.end) || points.some((r) => n === r.start);
}

/** Which ops the selection takes, in op order. Numbering the base side, a replacement's additions
 * follow its deletions, so they are taken with the deletion directly before them. */
export function selectOps(ops: readonly DiffOp[], ranges: readonly LineRange[], side: Side): boolean[] {
  const points = ranges.filter((r) => r.end < r.start);
  const real = ranges.filter((r) => r.end >= r.start);
  const out: boolean[] = [];
  ops.forEach((op, i) => {
    let take = selected(op, real, points, side);
    if (!take && side === "base" && op.kind === "add") {
      const prev = ops[i - 1];
      take = prev !== undefined && prev.kind !== "eq" && out[i - 1] === true;
    }
    out.push(take);
  });
  return out;
}

/** The change blocks (maximal runs of non-`eq` ops) as `[first, last]` op indexes. */
export function changeBlocks(ops: readonly DiffOp[]): [number, number][] {
  const out: [number, number][] = [];
  let from = -1;
  ops.forEach((op, i) => {
    if (op.kind !== "eq") {
      if (from < 0) from = i;
    } else if (from >= 0) {
      out.push([from, i - 1]);
      from = -1;
    }
  });
  if (from >= 0) out.push([from, ops.length - 1]);
  return out;
}

/** The ranges of a whole change block, numbered on `side`, for staging one hunk. */
export function blockRange(ops: readonly DiffOp[], block: [number, number], side: Side): LineRange {
  let start = Number.POSITIVE_INFINITY;
  let end = 0;
  let pos = 0;
  for (let i = block[0]; i <= block[1]; i++) {
    const op = ops[i];
    if (!op) continue;
    const own = side === "target" ? op.kind === "add" : op.kind === "del";
    const n = side === "target" ? op.targetNo : op.baseNo;
    pos = pos === 0 ? n : pos;
    if (own) {
      start = Math.min(start, n);
      end = Math.max(end, n);
    }
  }
  return start === Number.POSITIVE_INFINITY ? { start: pos, end: pos - 1 } : { start, end };
}

const CONTEXT = 3;

/** C-style quoting git uses for odd path names in patch headers. */
export function quotePath(p: string): string {
  const plain = [...p].every((c) => {
    const k = c.codePointAt(0) ?? 0;
    return k > 0x1f && k < 0x7f && c !== '"' && c !== "\\";
  });
  if (plain) return p;
  let out = "";
  for (const b of new TextEncoder().encode(p)) {
    if (b === 0x22) out += '\\"';
    else if (b === 0x5c) out += "\\\\";
    else if (b === 0x0a) out += "\\n";
    else if (b === 0x09) out += "\\t";
    else if (b < 0x20 || b >= 0x7f) out += `\\${b.toString(8).padStart(3, "0")}`;
    else out += String.fromCharCode(b);
  }
  return `"${out}"`;
}

function lineOut(prefix: string, text: string): string {
  return text.endsWith(NO_EOL)
    ? `${prefix}${text.slice(0, -NO_EOL.length)}\n\\ No newline at end of file\n`
    : `${prefix}${text}`;
}

/** A `git apply --cached` patch that moves `base` towards `target` by the selected ops only, or
 * null when the selection is empty. `base` must be what the index holds. Unselected deletions stay
 * as context and unselected additions are left out. */
export function buildPatch(
  path: string,
  base: string,
  target: string,
  ranges: readonly LineRange[],
  side: Side,
): string | null {
  const ops = diffLines(splitLines(base), splitLines(target));
  const take = selectOps(ops, ranges, side);
  return patchFromOps(path, ops, take);
}

/** The patch for one change block (a hunk), by its index in `changeBlocks`. */
export function buildHunkPatch(
  path: string,
  base: string,
  target: string,
  blockIndex: number,
): string | null {
  const ops = diffLines(splitLines(base), splitLines(target));
  const block = changeBlocks(ops)[blockIndex];
  if (!block) return null;
  const take = ops.map((_, i) => i >= block[0] && i <= block[1]);
  return patchFromOps(path, ops, take);
}

export function patchFromOps(path: string, ops: readonly DiffOp[], take: readonly boolean[]): string | null {
  if (!take.some(Boolean)) return null;
  // Effective ops: what the patch says about each line.
  const eff: { kind: OpKind; text: string }[] = [];
  ops.forEach((op, i) => {
    if (op.kind === "eq") eff.push({ kind: "eq", text: op.text });
    else if (op.kind === "del") eff.push({ kind: take[i] ? "del" : "eq", text: op.text });
    else if (take[i]) eff.push({ kind: "add", text: op.text });
  });
  const changed = eff.flatMap((e, i) => (e.kind === "eq" ? [] : [i]));
  // Group changes whose context would overlap into one hunk.
  const groups: [number, number][] = [];
  for (const i of changed) {
    const last = groups[groups.length - 1];
    if (last && i - last[1] - 1 <= CONTEXT * 2) last[1] = i;
    else groups.push([i, i]);
  }
  const aName = quotePath(`a/${path}`);
  const bName = quotePath(`b/${path}`);
  let out = `diff --git ${aName} ${bName}\n--- ${aName}\n+++ ${bName}\n`;
  // Running line numbers before each effective op.
  const oldBefore: number[] = [];
  const newBefore: number[] = [];
  let o = 0;
  let n = 0;
  for (const e of eff) {
    oldBefore.push(o);
    newBefore.push(n);
    if (e.kind !== "add") o++;
    if (e.kind !== "del") n++;
  }
  for (const [first, last] of groups) {
    const from = Math.max(0, first - CONTEXT);
    const to = Math.min(eff.length - 1, last + CONTEXT);
    let oldLen = 0;
    let newLen = 0;
    let body = "";
    for (let i = from; i <= to; i++) {
      const e = eff[i];
      if (!e) continue;
      if (e.kind !== "add") oldLen++;
      if (e.kind !== "del") newLen++;
      body += lineOut(e.kind === "eq" ? " " : e.kind === "del" ? "-" : "+", e.text);
    }
    const oldStart = oldLen === 0 ? oldBefore[from] ?? 0 : (oldBefore[from] ?? 0) + 1;
    const newStart = newLen === 0 ? newBefore[from] ?? 0 : (newBefore[from] ?? 0) + 1;
    out += `@@ -${oldStart}${oldLen === 1 ? "" : `,${oldLen}`} +${newStart}${newLen === 1 ? "" : `,${newLen}`} @@\n${body}`;
  }
  return out;
}
