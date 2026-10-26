// Affected-projects list (SPEC §24.3.5, `Workspace.Affected` / `Projects.Affected`).
export type AffectedScope = "uncommitted" | "change" | "stack";

export interface AffectedRow {
  readonly id: string;
  readonly name: string;
  readonly root: string;
  readonly direct: boolean;
  readonly reasons: readonly string[];
  /** target name -> argv (the host runs it; asc runs nothing itself). */
  readonly targets: Readonly<Record<string, readonly string[]>>;
}

export function sortAffected(rows: readonly AffectedRow[]): AffectedRow[] {
  return rows.toSorted(
    (a, b) => Number(b.direct) - Number(a.direct) || a.name.localeCompare(b.name),
  );
}

export function filterAffected(rows: readonly AffectedRow[], query: string): AffectedRow[] {
  const q = query.trim().toLowerCase();
  return q === ""
    ? [...rows]
    : rows.filter((r) => r.name.toLowerCase().includes(q) || r.root.toLowerCase().includes(q));
}

export function scopeLabel(s: AffectedScope): string {
  if (s === "uncommitted") return "Uncommitted changes";
  return s === "change" ? "This change" : "This stack";
}
