// View models. Screens use these and never see protobuf messages (docs/WEB-UI.md section 4.1).

export interface RefView {
  /** Full name, `refs/heads/main`. */
  name: string;
  /** Name without the namespace, `main`. */
  short: string;
  kind: "branch" | "tag" | "other";
  /** Hex id of the target. */
  id: string;
}

export interface RefsView {
  /** Short name of the default branch (the symbolic HEAD). */
  defaultBranch: string;
  refs: readonly RefView[];
}

export type EntryKind = "dir" | "file" | "exec" | "symlink" | "submodule";

export interface TreeRow {
  name: string;
  path: string;
  kind: EntryKind;
  size: number;
}

export interface TreeView {
  rows: readonly TreeRow[];
  /** The stream has ended. */
  done: boolean;
}

export interface CommitView {
  id: string;
  short: string;
  subject: string;
  author: string;
  /** Seconds since the epoch. */
  time: number;
}

export interface FileView {
  /** Lines so far. The array grows in place while `done` is false; read `count`. */
  lines: string[];
  count: number;
  size: number;
  binary: boolean;
  truncated: boolean;
  done: boolean;
}

export interface ReviewBadge {
  /** `open`, `merged`, `closed` or the forge's own word. */
  state: string;
  number: number;
  draft: boolean;
  approvals: number;
  /** `success`, `failure`, `pending` or empty. */
  checks: string;
  url: string;
}

/** What a change description says about itself (see parseDescription). */
export interface ChangeDescription {
  /** First line. */
  title: string;
  /** The prose between the title and the trailers: why the change exists. */
  intent: string;
  /** `Risk:` trailer: what the reviewer should look at. */
  risk: string;
  /** `Risk-area:` trailer, one word such as `billing`. */
  riskArea: string;
  /** `Agent:` trailer: the agent run that wrote the change, empty for a person. */
  agent: string;
}

export interface DiffTotals {
  added: number;
  removed: number;
  files: number;
}

export interface ChangeRow {
  /** Change id in k-z form. */
  id: string;
  short: string;
  title: string;
  version: number;
  /** 0 is the bottom of its stack. */
  position: number;
  stackSize: number;
  /** The branch or user ref the stack was found under. */
  ref: string;
  landed: boolean;
  divergent: boolean;
  needsRestack: boolean;
  review: ReviewBadge | undefined;
  /** Who wrote the newest version. */
  author: string;
  /** Seconds since the epoch of the newest version. */
  time: number;
  description: ChangeDescription;
  stat: DiffTotals | undefined;
}

export interface StackView {
  ref: string;
  trunk: string;
  changes: readonly ChangeRow[];
}

export interface ChangeListView {
  trunk: string;
  stacks: readonly StackView[];
}
