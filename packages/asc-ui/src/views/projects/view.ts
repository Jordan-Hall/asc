// Project graph as inline SVG; names only through `textContent`.
import { layoutGraph, neighbourhood, type ProjectNode } from "./model";

const NS = "http://www.w3.org/2000/svg";
const W = 140;
const H = 28;
const GX = 40;
const GY = 24;

export interface ProjectsView {
  update(projects: readonly ProjectNode[]): void;
  dispose(): void;
}

export function mountProjects(
  container: HTMLElement,
  projects: readonly ProjectNode[],
  onOpen?: (id: string) => void,
): ProjectsView {
  const doc = container.ownerDocument;
  const root = doc.createElement("div");
  root.className = "asc-projects";
  root.setAttribute("data-testid", "projects");
  container.append(root);
  let current = projects;
  let focus: string | undefined;

  function render(): void {
    root.replaceChildren();
    const layout = layoutGraph(current);
    if (layout.nodes.length === 0) {
      root.textContent = "No projects";
      return;
    }
    const keep = focus ? neighbourhood(current, focus) : undefined;
    const pos = new Map(
      layout.nodes.map((n) => [n.node.id, { x: n.layer * (W + GX), y: n.index * (H + GY) }]),
    );
    const rowsMax = Math.max(...layout.nodes.map((n) => n.index)) + 1;
    const svg = doc.createElementNS(NS, "svg");
    svg.setAttribute("width", String(layout.layers * (W + GX)));
    svg.setAttribute("height", String(rowsMax * (H + GY)));
    for (const e of layout.edges) {
      const a = pos.get(e.from);
      const b = pos.get(e.to);
      if (!a || !b) continue;
      const line = doc.createElementNS(NS, "line");
      line.setAttribute("x1", String(a.x));
      line.setAttribute("y1", String(a.y + H / 2));
      line.setAttribute("x2", String(b.x + W));
      line.setAttribute("y2", String(b.y + H / 2));
      line.setAttribute("stroke", "var(--asc-border)");
      line.setAttribute("data-edge", `${e.from}>${e.to}`);
      if (keep && !(keep.has(e.from) && keep.has(e.to))) line.setAttribute("opacity", "0.15");
      svg.append(line);
    }
    for (const n of layout.nodes) {
      const p = pos.get(n.node.id);
      if (!p) continue;
      const cyc = layout.cyclic.includes(n.node.id);
      const g = doc.createElementNS(NS, "g");
      g.setAttribute("transform", `translate(${p.x},${p.y})`);
      g.setAttribute("data-testid", `node-${n.node.id}`);
      g.setAttribute("tabindex", "0");
      if (keep && !keep.has(n.node.id)) g.setAttribute("opacity", "0.3");
      if (cyc) g.setAttribute("data-cyclic", "true");
      const rect = doc.createElementNS(NS, "rect");
      rect.setAttribute("width", String(W));
      rect.setAttribute("height", String(H));
      rect.setAttribute("rx", "4");
      rect.setAttribute("fill", "var(--asc-badge-bg)");
      rect.setAttribute("stroke", cyc ? "var(--asc-error)" : "var(--asc-border)");
      const text = doc.createElementNS(NS, "text");
      text.setAttribute("x", "6");
      text.setAttribute("y", String(H / 2 + 4));
      text.setAttribute("fill", "var(--asc-badge-fg)");
      text.textContent = n.node.name;
      g.append(rect, text);
      g.addEventListener("click", () => {
        focus = focus === n.node.id ? undefined : n.node.id;
        render();
      });
      g.addEventListener("dblclick", () => onOpen?.(n.node.id));
      svg.append(g);
    }
    root.append(svg);
  }
  render();
  return {
    update(p) {
      current = p;
      focus = undefined;
      render();
    },
    dispose() {
      root.remove();
    },
  };
}
