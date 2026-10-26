// Split dialog model (SPEC §24.3.2): parse the `Repo.Diff` text of one change, assign changed lines
// (or whole files) to pieces, and build the `SplitSpec` that `asc split --hunks <spec>` takes (P46).
// Pure state and rules, no DOM. Pieces are bottom first; each piece's patch applies to the tree of
// the piece below; the last piece is the remainder and carries no patch.

export type LineKind = "ctx" | "add" | "del";
export interface DiffLine {
  readonly kind: LineKind;
  readonly text: string;
}
export interface Hunk {
  readonly oldStart: number;
  readonly oldLen: number;
  readonly newStart: number;
  readonly newLen: number;
  readonly lines: readonly DiffLine[];
}
export interface SplitFile {
  /** Path (new side), forward slashes, safe to name in `wholeFiles`. */
  readonly path: string;
  /** Hunks and lines can be picked; otherwise the file moves whole (binary, rename, mode, add, delete). */
  readonly granular: boolean;
  /** Why the file cannot be split at all; the split is blocked while one exists. */
  readonly unsupported?: string;
  readonly hunks: readonly Hunk[];
}

/** Wire shape of `SplitSpec` (proto3 JSON). `patch` is standard base64 of a git-apply patch. */
export interface SplitPieceSpec {
  readonly message?: string;
  readonly patch?: string;
  readonly wholeFiles?: readonly string[];
}
export interface SplitSpec {
  readonly pieces: readonly SplitPieceSpec[];
}

/** Limits mirror `crates/asc/src/stack/cli/split_hunks.rs`. */
export const MAX_PIECES = 64;
export const MAX_MESSAGE = 1024 * 1024;
export const MAX_FILES = 10_000;
const MAX_PATCH = 16 * 1024 * 1024;

/** A path a patch header and `wholeFiles` can carry unquoted and the engine will accept. */
export function safePath(p: string): boolean {
  if (p === "" || p.startsWith("/") || p.endsWith("/")) return false;
  for (let i = 0; i < p.length; i++) {
    const c = p.charCodeAt(i);
    if (c < 0x20 || c === 0x7f || c === 0x22 || c === 0x5c) return false;
  }
  return p.split("/").every((s) => s !== "" && s !== "." && s !== "..");
}

const HUNK = /^@@ -(\d+)(?:,(\d+))? \+(\d+)(?:,(\d+))? @@/;
const INDEX = /^index [0-9a-f]+\.\.[0-9a-f]+(?: \d+)?$/;

/** Parse git-format diff text into files. Never throws; anything unexpected is marked unsupported. */
export function parseDiff(text: string): SplitFile[] {
  const files: SplitFile[] = [];
  if (text === "") return files;
  const lines = text.split("\n");
  if (lines[lines.length - 1] === "") lines.pop();
  const at = (n: number): string => lines[n] ?? "";
  let i = 0;
  while (i < lines.length) {
    if (!at(i).startsWith("diff --git ")) {
      i++;
      continue;
    }
    const header: string[] = [at(i)];
    i++;
    while (i < lines.length && !at(i).startsWith("diff --git ") && !at(i).startsWith("@@")) {
      header.push(at(i));
      i++;
    }
    const hunks: Hunk[] = [];
    let noEol = false;
    let bad = false;
    while (i < lines.length && !at(i).startsWith("diff --git ")) {
      const m = HUNK.exec(at(i));
      if (!m) {
        bad = true;
        i++;
        continue;
      }
      const oldLen = m[2] === undefined ? 1 : Number(m[2]);
      const newLen = m[4] === undefined ? 1 : Number(m[4]);
      const hl: DiffLine[] = [];
      i++;
      while (i < lines.length) {
        const l = at(i);
        const c = l[0];
        if (c === "\\") noEol = true;
        else if (c === " ") hl.push({ kind: "ctx", text: l.slice(1) });
        else if (c === "+") hl.push({ kind: "add", text: l.slice(1) });
        else if (c === "-") hl.push({ kind: "del", text: l.slice(1) });
        else break;
        i++;
      }
      const o = hl.filter((x) => x.kind !== "add").length;
      const n = hl.filter((x) => x.kind !== "del").length;
      if (o !== oldLen || n !== newLen) bad = true;
      hunks.push({ oldStart: Number(m[1]), oldLen, newStart: Number(m[3]), newLen, lines: hl });
    }
    files.push(describe(header, hunks, noEol, bad));
  }
  return files;
}

function describe(header: string[], hunks: Hunk[], noEol: boolean, bad: boolean): SplitFile {
  const first = header[0] ?? "";
  const dm = /^diff --git a\/(.+) b\/(.+)$/.exec(first);
  const rest = header.slice(1);
  const plain = rest.every((l) => INDEX.test(l) || l.startsWith("--- ") || l.startsWith("+++ "));
  const minus = rest.find((l) => l.startsWith("--- "));
  const plus = rest.find((l) => l.startsWith("+++ "));
  const renamed = rest.find((l) => l.startsWith("rename to "));
  const path = renamed ? renamed.slice("rename to ".length) : (dm?.[2] ?? "");
  const unsupported = (why: string): SplitFile => ({
    path,
    granular: false,
    unsupported: why,
    hunks: [],
  });
  if (!dm || path === "" || first.includes('"') || !safePath(path))
    return unsupported("Unsupported path");
  if (bad) return unsupported("Unparsable diff");
  const granular =
    plain &&
    dm[1] === dm[2] &&
    minus === `--- a/${path}` &&
    plus === `+++ b/${path}` &&
    hunks.length > 0 &&
    !noEol &&
    hunks.every((h) => h.oldLen > 0 || h.newLen > 0);
  return { path, granular, hunks: granular ? hunks : [] };
}

export interface SplitState {
  readonly files: readonly SplitFile[];
  /** Number of pieces (2..MAX_PIECES); the last is the remainder. */
  readonly pieces: number;
  /** Message per piece; the last may stay empty (the original message is kept). */
  readonly messages: readonly string[];
  /** Explicit piece per line (`f:h:l`) or whole file (`f`); everything else is in the last piece. */
  readonly assign: Readonly<Record<string, number>>;
  /** The diff was cut short; splitting would silently lose changes. */
  readonly truncated: boolean;
}

export function initSplit(diffText: string, truncated: boolean, subject: string): SplitState {
  return { files: parseDiff(diffText), pieces: 2, messages: ["", subject], assign: {}, truncated };
}

export const lineKey = (f: number, h: number, l: number): string => `${f}:${h}:${l}`;

export function pieceOfLine(s: SplitState, f: number, h: number, l: number): number {
  return s.assign[lineKey(f, h, l)] ?? s.pieces - 1;
}
export function pieceOfFile(s: SplitState, f: number): number {
  return s.assign[String(f)] ?? s.pieces - 1;
}

const isChange = (l: DiffLine): boolean => l.kind !== "ctx";

function withAssign(s: SplitState, set: ReadonlyArray<readonly [string, number]>): SplitState {
  const assign: Record<string, number> = { ...s.assign };
  for (const [k, p] of set) {
    if (p >= s.pieces - 1) delete assign[k];
    else assign[k] = p;
  }
  return { ...s, assign };
}

/** Move one changed line to `piece` (the last piece means "leave in the remainder"). */
export function assignLine(
  s: SplitState,
  f: number,
  h: number,
  l: number,
  piece: number,
): SplitState {
  const line = s.files[f]?.hunks[h]?.lines[l];
  if (!line || !isChange(line) || !s.files[f]?.granular || piece < 0 || piece >= s.pieces) return s;
  return withAssign(s, [[lineKey(f, h, l), piece]]);
}

export function assignHunk(s: SplitState, f: number, h: number, piece: number): SplitState {
  const hunk = s.files[f]?.hunks[h];
  if (!hunk || !s.files[f]?.granular || piece < 0 || piece >= s.pieces) return s;
  return withAssign(
    s,
    hunk.lines.flatMap((x, l) => (isChange(x) ? [[lineKey(f, h, l), piece] as const] : [])),
  );
}

/** Move a whole file: every changed line of a granular file, or the file itself otherwise. */
export function assignFile(s: SplitState, f: number, piece: number): SplitState {
  const file = s.files[f];
  if (!file || file.unsupported || piece < 0 || piece >= s.pieces) return s;
  if (!file.granular) return withAssign(s, [[String(f), piece]]);
  const set: Array<readonly [string, number]> = [];
  file.hunks.forEach((hk, h) => {
    hk.lines.forEach((x, l) => {
      if (isChange(x)) set.push([lineKey(f, h, l), piece]);
    });
  });
  return withAssign(s, set);
}

/** Add a piece above the lower ones; the old remainder keeps its lines as a lower piece (empty until filled). */
export function addPiece(s: SplitState): SplitState {
  if (s.pieces >= MAX_PIECES) return s;
  const old = s.pieces - 1;
  return {
    ...s,
    pieces: s.pieces + 1,
    messages: [...s.messages.slice(0, old), "", s.messages[old] ?? ""],
  };
}

/** Remove a lower piece; its lines fall to the remainder. */
export function removePiece(s: SplitState, piece: number): SplitState {
  if (s.pieces <= 2 || piece < 0 || piece >= s.pieces - 1) return s;
  const assign: Record<string, number> = {};
  for (const [k, p] of Object.entries(s.assign)) {
    if (p === piece) continue;
    assign[k] = p > piece ? p - 1 : p;
  }
  return { ...s, pieces: s.pieces - 1, messages: s.messages.filter((_, i) => i !== piece), assign };
}

export function setPieceMessage(s: SplitState, piece: number, message: string): SplitState {
  if (piece < 0 || piece >= s.pieces) return s;
  return { ...s, messages: s.messages.map((m, i) => (i === piece ? message : m)) };
}

/** Changed lines (or whole files) per piece. */
export function pieceCounts(s: SplitState): number[] {
  const out = Array.from({ length: s.pieces }, () => 0);
  const bump = (p: number): void => {
    out[p] = (out[p] ?? 0) + 1;
  };
  s.files.forEach((file, f) => {
    if (file.unsupported) return;
    if (!file.granular) {
      bump(pieceOfFile(s, f));
      return;
    }
    file.hunks.forEach((hk, h) => {
      hk.lines.forEach((x, l) => {
        if (isChange(x)) bump(pieceOfLine(s, f, h, l));
      });
    });
  });
  return out;
}

/** Why the split cannot run; undefined when it can. */
export function commitBlockedReason(s: SplitState): string | undefined {
  if (s.truncated) return "The diff is truncated";
  if (s.files.length === 0) return "Nothing to split";
  if (s.files.length > MAX_FILES) return "Too many files";
  const bad = s.files.find((f) => f.unsupported);
  if (bad) return `${bad.path || "A file"}: ${bad.unsupported}`;
  const empty = pieceCounts(s).findIndex((c) => c === 0);
  if (empty >= 0) return `Piece ${empty + 1} has no changes`;
  for (let i = 0; i < s.pieces - 1; i++) {
    if ((s.messages[i] ?? "").trim() === "") return `Piece ${i + 1} needs a message`;
  }
  if (s.messages.some((m) => m.length > MAX_MESSAGE)) return "Message too long";
  return undefined;
}

/** The git-apply patch of piece `k` over the tree of the piece below, or "" when it changes nothing. */
export function buildPatch(s: SplitState, k: number): string {
  let out = "";
  s.files.forEach((file, f) => {
    if (!file.granular || file.unsupported) return;
    let delta = 0; // line shift of the base against the original, from lower pieces
    let pdelta = 0; // shift made by this piece's own earlier hunks
    let body = "";
    file.hunks.forEach((hk, h) => {
      let pos = (hk.oldLen === 0 ? hk.oldStart + 1 : hk.oldStart) + delta;
      let firstPos = -1;
      let oldLen = 0;
      let newLen = 0;
      let net = 0;
      let lowNet = 0;
      let text = "";
      let changed = false;
      const emit = (prefix: string, t: string): void => {
        if (firstPos < 0) firstPos = pos;
        text += `${prefix}${t}\n`;
      };
      const keep = (t: string): void => {
        emit(" ", t);
        oldLen++;
        newLen++;
        pos++;
      };
      hk.lines.forEach((x, l) => {
        if (x.kind === "ctx") return keep(x.text);
        const p = pieceOfLine(s, f, h, l);
        if (x.kind === "add") {
          if (p < k) {
            lowNet++;
            keep(x.text); // already in the base
          } else if (p === k) {
            emit("+", x.text);
            newLen++;
            net++;
            changed = true;
          } // later pieces: not in the base yet
        } else if (p < k) {
          lowNet--; // already gone from the base
        } else if (p === k) {
          emit("-", x.text);
          oldLen++;
          pos++;
          net--;
          changed = true;
        } else {
          keep(x.text); // still there
        }
      });
      if (changed) {
        const newPos = firstPos + pdelta;
        const os = oldLen === 0 ? firstPos - 1 : firstPos;
        const ns = newLen === 0 ? newPos - 1 : newPos;
        body += `@@ -${os},${oldLen} +${ns},${newLen} @@\n${text}`;
        pdelta += net;
      }
      delta += lowNet;
    });
    if (body !== "")
      out += `diff --git a/${file.path} b/${file.path}\n--- a/${file.path}\n+++ b/${file.path}\n${body}`;
  });
  return out;
}

function b64(text: string): string {
  const bytes = new TextEncoder().encode(text);
  let bin = "";
  for (let i = 0; i < bytes.length; i += 0x8000)
    bin += String.fromCharCode(...bytes.subarray(i, i + 0x8000));
  return btoa(bin);
}

/** The spec for `split --hunks`; throws when {@link commitBlockedReason} says it cannot run. */
export function toSpec(s: SplitState): SplitSpec {
  const why = commitBlockedReason(s);
  if (why !== undefined) throw new Error(why);
  const pieces: SplitPieceSpec[] = [];
  for (let k = 0; k < s.pieces - 1; k++) {
    const patch = buildPatch(s, k);
    if (patch.length > MAX_PATCH) throw new Error(`Piece ${k + 1} is too large`);
    const whole = s.files.flatMap((f, i) =>
      !f.granular && pieceOfFile(s, i) === k ? [f.path] : [],
    );
    pieces.push({
      message: s.messages[k] ?? "",
      ...(patch !== "" ? { patch: b64(patch) } : {}),
      ...(whole.length ? { wholeFiles: whole } : {}),
    });
  }
  const lastMsg = s.messages[s.pieces - 1] ?? "";
  pieces.push(lastMsg.trim() === "" ? {} : { message: lastMsg });
  return { pieces };
}

const CHANGE_ID = /^[A-Za-z0-9]{4,64}$/;

/** A `Bridge.Run` request body: the verb and its args (flags `--k=v`, then positionals). */
export interface RunSpec {
  readonly verb: string;
  readonly args: readonly string[];
}

/** The run for the host once it has written the spec to the plugin tmp file `file`. Never `--by-hunk` (interactive). */
export function splitRun(change: string, file: string): RunSpec {
  if (!CHANGE_ID.test(change)) throw new Error("Invalid change id");
  return { verb: "split", args: [`--hunks=${file}`, change] };
}
