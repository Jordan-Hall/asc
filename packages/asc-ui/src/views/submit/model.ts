import type { RunSpec } from "../split/model";

// Submit dialog model (SPEC §24.3.2): options for `asc submit [-s] [--draft|--publish]
// [--reviewer <u>…] [--auto-merge]` and the PR title and description edits sent as a Forgejo
// `PATCH …/pulls/{n}` (allowlisted body: `title`, `body`). Pure state and rules, no DOM.

export type SubmitMode = "default" | "draft" | "publish";

/** Limits mirror `crates/asc/src/stack/cli/submit.rs`. */
export const MAX_REVIEWERS = 32;
export const MAX_REVIEWER_LEN = 40;
export const MAX_TITLE = 255;
export const MAX_BODY = 64 * 1024;

/** A reviewer handle the CLI accepts: `[A-Za-z0-9._-]`, not starting with a dot or a dash. */
export function validReviewer(name: string): boolean {
  return /^[A-Za-z0-9_][A-Za-z0-9._-]*$/.test(name) && name.length <= MAX_REVIEWER_LEN;
}

export interface PullText {
  readonly title: string;
  readonly body: string;
}

/** A change in the submit range; `pull` is its existing PR (number and current text), if any. */
export interface SubmitChange {
  readonly id: string;
  /** The change message; the PR text defaults to it (first line title, rest description). */
  readonly message: string;
  readonly pull?: { readonly number: number; readonly title: string; readonly body: string; readonly draft: boolean };
}

export function defaultPullText(message: string): PullText {
  const text = message.replace(/\r\n?/g, "\n").trim();
  const nl = text.indexOf("\n");
  return nl < 0
    ? { title: text, body: "" }
    : { title: text.slice(0, nl).trim(), body: text.slice(nl + 1).trim() };
}

export interface SubmitState {
  readonly changes: readonly SubmitChange[];
  /** Logins Forgejo lists as assignable; reviewers must come from here once it is loaded. */
  readonly candidates: readonly string[] | undefined;
  readonly stack: boolean;
  readonly mode: SubmitMode;
  readonly reviewers: readonly string[];
  readonly autoMerge: boolean;
  /** Edited PR text per change id; absent means unchanged. */
  readonly text: Readonly<Record<string, PullText>>;
}

export function initSubmit(changes: readonly SubmitChange[], candidates?: readonly string[]): SubmitState {
  return { changes, candidates, stack: true, mode: "default", reviewers: [], autoMerge: false, text: {} };
}

export const setMode = (s: SubmitState, mode: SubmitMode): SubmitState => ({ ...s, mode });
export const setStack = (s: SubmitState, stack: boolean): SubmitState => ({ ...s, stack });
export const setAutoMerge = (s: SubmitState, autoMerge: boolean): SubmitState => ({ ...s, autoMerge });
export const setCandidates = (s: SubmitState, candidates: readonly string[] | undefined): SubmitState => ({
  ...s,
  candidates,
});

/** Why `name` cannot be a reviewer; undefined when it can. */
export function reviewerBlockedReason(s: SubmitState, name: string): string | undefined {
  if (!validReviewer(name)) return "Not a valid user name";
  if (s.candidates === undefined) return "Reviewers are not loaded";
  if (!s.candidates.some((c) => c.toLowerCase() === name.toLowerCase())) return "Not assignable in this repository";
  if (s.reviewers.some((r) => r.toLowerCase() === name.toLowerCase())) return "Already added";
  if (s.reviewers.length >= MAX_REVIEWERS) return "Too many reviewers";
  return undefined;
}

export function addReviewer(s: SubmitState, name: string): SubmitState {
  if (reviewerBlockedReason(s, name) !== undefined) return s;
  const login = s.candidates?.find((c) => c.toLowerCase() === name.toLowerCase()) ?? name;
  return { ...s, reviewers: [...s.reviewers, login] };
}

export const removeReviewer = (s: SubmitState, name: string): SubmitState => ({
  ...s,
  reviewers: s.reviewers.filter((r) => r !== name),
});

/** The text the dialog shows for a change: the edit, else the PR's own, else the change message. */
export function textOf(s: SubmitState, id: string): PullText {
  const edited = s.text[id];
  if (edited) return edited;
  const c = s.changes.find((x) => x.id === id);
  if (!c) return { title: "", body: "" };
  return c.pull ? { title: c.pull.title, body: c.pull.body } : defaultPullText(c.message);
}

export function setText(s: SubmitState, id: string, text: PullText): SubmitState {
  if (!s.changes.some((c) => c.id === id)) return s;
  return { ...s, text: { ...s.text, [id]: text } };
}

/** The `PATCH` body for an existing PR whose text was edited: only `title` and `body`, only what changed. */
export interface PullPatch {
  readonly title?: string;
  readonly body?: string;
}
export interface PullEdit {
  readonly change: string;
  readonly number: number;
  readonly patch: PullPatch;
}

export function pullEdits(s: SubmitState): PullEdit[] {
  const out: PullEdit[] = [];
  for (const c of s.changes) {
    const edited = s.text[c.id];
    if (!edited || !c.pull) continue;
    const patch: { title?: string; body?: string } = {};
    if (edited.title !== c.pull.title) patch.title = edited.title;
    if (edited.body !== c.pull.body) patch.body = edited.body;
    if (patch.title !== undefined || patch.body !== undefined) out.push({ change: c.id, number: c.pull.number, patch });
  }
  return out;
}

/** Why the submit cannot run; undefined when it can. */
export function submitBlockedReason(s: SubmitState): string | undefined {
  if (s.changes.length === 0) return "Nothing to submit";
  if (s.autoMerge && s.mode === "draft") return "A draft cannot auto-merge";
  if (s.reviewers.length > MAX_REVIEWERS) return "Too many reviewers";
  if (s.reviewers.some((r) => !validReviewer(r))) return "Not a valid user name";
  for (const c of s.changes) {
    const t = s.text[c.id];
    if (!t) continue;
    if (t.title.trim() === "") return "A PR title cannot be empty";
    if (t.title.length > MAX_TITLE) return "A PR title is too long";
    if (/[\r\n]/.test(t.title)) return "A PR title is one line";
    if (t.body.length > MAX_BODY) return "A PR description is too long";
  }
  return undefined;
}

const CHANGE_ID = /^[A-Za-z0-9]{4,64}$/;

/** The `Bridge.Run` request for `submit`; the host asks for modal confirmation before running it. */
export function submitRun(s: SubmitState, change?: string): RunSpec {
  const why = submitBlockedReason(s);
  if (why !== undefined) throw new Error(why);
  const args: string[] = [];
  if (s.stack) args.push("-s");
  if (s.mode === "draft") args.push("--draft");
  if (s.mode === "publish") args.push("--publish");
  for (const r of s.reviewers) args.push(`--reviewer=${r}`);
  if (s.autoMerge) args.push("--auto-merge");
  if (change !== undefined) {
    if (!CHANGE_ID.test(change)) throw new Error("Invalid change id");
    args.push(change);
  }
  return { verb: "submit", args };
}
