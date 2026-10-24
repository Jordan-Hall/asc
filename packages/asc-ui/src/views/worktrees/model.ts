// Worktrees tab model (SPEC §24.3.3): `Workspace.Worktrees` rows plus open/remove actions
// with a stated reason when disabled.

export interface WorktreeRow {
  readonly wsId: string;
  readonly path: string;
  readonly mode: string;
  readonly headRef: string;
  readonly dirty: number;
  readonly staged: number;
  readonly ahead: number;
  readonly behind: number;
  /** Lock reason; empty when unlocked. */
  readonly locked: string;
  readonly inProgress: string;
  readonly current: boolean;
  readonly main: boolean;
}

export type WorktreeAction = "open" | "remove";

export interface ActionState {
  readonly enabled: boolean;
  readonly reason?: string;
}

export function actionState(row: WorktreeRow, action: WorktreeAction): ActionState {
  if (action === "open") return row.current ? { enabled: false, reason: "Already open" } : { enabled: true };
  if (row.main) return { enabled: false, reason: "The main worktree cannot be removed" };
  if (row.current) return { enabled: false, reason: "Current worktree" };
  if (row.locked !== "") return { enabled: false, reason: `Locked: ${row.locked}` };
  if (row.inProgress !== "") return { enabled: false, reason: `In progress: ${row.inProgress}` };
  if (row.dirty > 0 || row.staged > 0) return { enabled: false, reason: "Uncommitted changes" };
  return { enabled: true };
}

export function summary(row: WorktreeRow): string {
  const parts: string[] = [];
  if (row.dirty > 0) parts.push(`${row.dirty} dirty`);
  if (row.staged > 0) parts.push(`${row.staged} staged`);
  if (row.ahead > 0) parts.push(`ahead ${row.ahead}`);
  if (row.behind > 0) parts.push(`behind ${row.behind}`);
  return parts.length === 0 ? "clean" : parts.join(", ");
}

/** Current first, then main, then by path. */
export function sortWorktrees(rows: readonly WorktreeRow[]): WorktreeRow[] {
  const rank = (r: WorktreeRow): number => (r.current ? 0 : r.main ? 1 : 2);
  return rows.toSorted((a, b) => rank(a) - rank(b) || a.path.localeCompare(b.path));
}
