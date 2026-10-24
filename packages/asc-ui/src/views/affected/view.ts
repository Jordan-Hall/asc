import type { Severity } from "../../platform";
import {
  type AffectedRow,
  type AffectedScope,
  filterAffected,
  scopeLabel,
  sortAffected,
} from "./model";

export interface AffectedHost {
  load(scope: AffectedScope, signal: AbortSignal): Promise<readonly AffectedRow[]>;
  /** Run a target's argv in the host terminal or task runner; asc runs nothing itself. */
  runTarget(row: AffectedRow, target: string, argv: readonly string[]): void;
  notify(severity: Severity, message: string): void;
}

export interface AffectedView {
  setScope(scope: AffectedScope): Promise<void>;
  dispose(): void;
}

export function mountAffected(
  container: HTMLElement,
  host: AffectedHost,
  initial: AffectedScope = "uncommitted",
): AffectedView {
  const doc = container.ownerDocument;
  const root = doc.createElement("div");
  root.className = "asc-affected";
  const bar = doc.createElement("div");
  bar.className = "asc-toolbar";
  const select = doc.createElement("select");
  select.setAttribute("aria-label", "Scope");
  for (const s of ["uncommitted", "change", "stack"] as const) {
    const o = doc.createElement("option");
    o.value = s;
    o.textContent = scopeLabel(s);
    select.append(o);
  }
  const search = doc.createElement("input");
  search.type = "search";
  search.placeholder = "Filter projects";
  search.setAttribute("aria-label", "Filter projects");
  bar.append(select, search);
  const status = doc.createElement("div");
  status.className = "asc-status";
  const list = doc.createElement("ul");
  list.className = "asc-rows";
  root.append(bar, status, list);
  container.append(root);

  let rows: readonly AffectedRow[] = [];
  let ctl = new AbortController();

  function render(): void {
    list.replaceChildren();
    const shown = filterAffected(sortAffected(rows), search.value);
    status.textContent = shown.length === 0 ? "No affected projects" : `${shown.length} affected`;
    for (const r of shown) {
      const li = doc.createElement("li");
      li.className = "asc-row";
      li.setAttribute("data-testid", `proj-${r.id}`);
      const name = doc.createElement("span");
      name.className = "asc-subject";
      name.textContent = r.name;
      const badge = doc.createElement("span");
      badge.className = r.direct ? "asc-badge" : "asc-badge asc-restack";
      badge.dataset.badge = r.direct ? "direct" : "indirect";
      badge.textContent = r.direct ? "direct" : "indirect";
      badge.title = r.reasons.join("\n");
      li.append(name, badge);
      for (const [t, argv] of Object.entries(r.targets)) {
        const b = doc.createElement("button");
        b.className = "asc-action";
        b.dataset.action = `run-${t}`;
        b.textContent = `Run ${t}`;
        b.addEventListener("click", () => host.runTarget(r, t, argv));
        li.append(b);
      }
      list.append(li);
    }
  }

  async function setScope(scope: AffectedScope): Promise<void> {
    ctl.abort();
    ctl = new AbortController();
    const mine = ctl;
    select.value = scope;
    status.textContent = "Loading...";
    try {
      const loaded = await host.load(scope, mine.signal);
      if (mine.signal.aborted) return;
      rows = loaded;
      render();
    } catch (e) {
      if (mine.signal.aborted) return;
      const m = e instanceof Error ? e.message : "request failed";
      status.textContent = m;
      host.notify("error", m);
    }
  }

  select.addEventListener("change", () => void setScope(select.value as AffectedScope));
  search.addEventListener("input", render);
  void setScope(initial);
  return {
    setScope,
    dispose() {
      ctl.abort();
      root.remove();
    },
  };
}
