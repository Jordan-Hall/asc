// Project graph (SPEC §24.3.5, `Projects.ProjectGraph`): layered layout of the dependency DAG.
export interface ProjectNode {
  readonly id: string;
  readonly name: string;
  readonly ecosystem?: string;
  /** ids of projects this one depends on. Unknown ids are ignored. */
  readonly deps: readonly string[];
}

export interface LayoutNode {
  readonly node: ProjectNode;
  readonly layer: number;
  readonly index: number;
}

export interface GraphLayout {
  readonly nodes: readonly LayoutNode[];
  readonly edges: readonly { readonly from: string; readonly to: string }[];
  readonly layers: number;
  /** Ids found on a dependency cycle (the cycle is broken where it was detected). */
  readonly cyclic: readonly string[];
}

/** Layer = longest dependency chain below the node, so dependencies sit on lower layers. */
export function layoutGraph(projects: readonly ProjectNode[]): GraphLayout {
  const byId = new Map(projects.map((p) => [p.id, p]));
  const depth = new Map<string, number>();
  const visiting = new Set<string>();
  const cyclic = new Set<string>();
  const visit = (id: string): number => {
    const d = depth.get(id);
    if (d !== undefined) return d;
    if (visiting.has(id)) {
      cyclic.add(id);
      return 0;
    }
    visiting.add(id);
    let best = -1;
    for (const dep of byId.get(id)?.deps ?? []) {
      if (byId.has(dep) && dep !== id) best = Math.max(best, visit(dep));
    }
    visiting.delete(id);
    depth.set(id, best + 1);
    return best + 1;
  };
  for (const p of projects) visit(p.id);
  const perLayer = new Map<number, number>();
  const nodes = projects
    .toSorted((a, b) => a.name.localeCompare(b.name) || a.id.localeCompare(b.id))
    .map((node): LayoutNode => {
      const layer = depth.get(node.id) ?? 0;
      const index = perLayer.get(layer) ?? 0;
      perLayer.set(layer, index + 1);
      return { node, layer, index };
    });
  const edges = projects.flatMap((p) =>
    p.deps.filter((d) => byId.has(d) && d !== p.id).map((d) => ({ from: p.id, to: d })),
  );
  return { nodes, edges, layers: perLayer.size, cyclic: [...cyclic].toSorted() };
}

/** The node plus everything it transitively depends on or is depended on by. */
export function neighbourhood(projects: readonly ProjectNode[], id: string): Set<string> {
  const out = new Set<string>([id]);
  const fwd = new Map<string, string[]>();
  const rev = new Map<string, string[]>();
  const push = (m: Map<string, string[]>, k: string, v: string) => {
    const l = m.get(k);
    if (l) l.push(v);
    else m.set(k, [v]);
  };
  for (const p of projects) {
    for (const d of p.deps) {
      push(fwd, p.id, d);
      push(rev, d, p.id);
    }
  }
  for (const m of [fwd, rev]) {
    const stack = [id];
    const seen = new Set<string>([id]);
    for (let cur = stack.pop(); cur !== undefined; cur = stack.pop()) {
      for (const n of m.get(cur) ?? []) {
        if (seen.has(n)) continue;
        seen.add(n);
        out.add(n);
        stack.push(n);
      }
    }
  }
  return out;
}
