// File decorations (SPEC 24.4.2): one provider; `onDidChangeFileDecorations` fires only with the
// URIs from the last `Status(paths)` delta. Colours are the `gitDecoration.*` theme colours.

export interface Decoration {
  badge: string;
  tooltip: string;
  /** A theme colour id. */
  color: string;
}

/** Maps a porcelain `xy` to a decoration, or null when clean. Index status wins only if the
 * worktree side is unchanged, since the worktree change is what the user acts on. */
export function decorationFor(xy: string): Decoration | null {
  const x = xy[0] ?? ".";
  const y = xy[1] ?? ".";
  if (xy === "??") return { badge: "U", tooltip: "Untracked", color: "gitDecoration.untrackedResourceForeground" };
  if (xy === "!!") return { badge: "I", tooltip: "Ignored", color: "gitDecoration.ignoredResourceForeground" };
  if (isConflict(xy)) {
    return { badge: "!", tooltip: "Conflict", color: "gitDecoration.conflictingResourceForeground" };
  }
  const c = y !== "." && y !== " " ? y : x;
  switch (c) {
    case "A":
      return { badge: "A", tooltip: "Added", color: "gitDecoration.addedResourceForeground" };
    case "D":
      return { badge: "D", tooltip: "Deleted", color: "gitDecoration.deletedResourceForeground" };
    case "R":
    case "C":
      return { badge: "R", tooltip: "Renamed", color: "gitDecoration.renamedResourceForeground" };
    case "M":
    case "T":
      return { badge: "M", tooltip: "Modified", color: "gitDecoration.modifiedResourceForeground" };
    default:
      return null;
  }
}

export function isConflict(xy: string): boolean {
  return xy === "DD" || xy === "AA" || xy.includes("U");
}

export interface DecorationSource {
  /** The xy for a repo-relative path, or undefined when clean. */
  xyFor(path: string): string | undefined;
}

/** Holds no state of its own: it reads the model and emits only the changed paths. */
export class DecorationProvider {
  private listeners = new Set<(paths: string[]) => void>();
  constructor(private readonly source: DecorationSource) {}
  decorate(path: string): Decoration | null {
    const xy = this.source.xyFor(path);
    return xy === undefined ? null : decorationFor(xy);
  }
  onDidChange(fn: (paths: string[]) => void): { dispose(): void } {
    this.listeners.add(fn);
    return { dispose: () => void this.listeners.delete(fn) };
  }
  /** Targeted invalidation: only these paths are re-queried. */
  invalidate(paths: string[]): void {
    if (paths.length === 0) return;
    for (const l of this.listeners) l(paths);
  }
}
