// Worktrees tab DOM view. Text only through `textContent`.
import { actionState, sortWorktrees, summary, type WorktreeRow } from "./model";

export interface WorktreesHost {
  open(path: string): void;
  /** Destructive: the host shows its native modal confirmation before running `worktree remove`. */
  remove(path: string): void;
  add(): void;
}

export interface WorktreesView {
  update(rows: readonly WorktreeRow[]): void;
  dispose(): void;
}

function badge(doc: Document, kind: string, text?: string): HTMLElement {
  const b = doc.createElement("span");
  b.className = "asc-badge";
  b.dataset["badge"] = kind;
  b.textContent = text ?? kind;
  return b;
}

export function mountWorktrees(
  container: HTMLElement,
  host: WorktreesHost,
  initial: readonly WorktreeRow[],
): WorktreesView {
  const doc = container.ownerDocument;
  const root = doc.createElement("div");
  root.className = "asc-worktrees";
  root.dataset["testid"] = "worktrees";
  container.append(root);

  const render = (rows: readonly WorktreeRow[]): void => {
    root.replaceChildren();
    const bar = doc.createElement("div");
    bar.className = "asc-toolbar";
    const add = doc.createElement("button");
    add.className = "asc-action";
    add.dataset["action"] = "add";
    add.textContent = "Add worktree";
    add.addEventListener("click", () => host.add());
    bar.append(add);
    root.append(bar);
    const list = doc.createElement("ul");
    list.className = "asc-rows";
    for (const row of sortWorktrees(rows)) {
      const li = doc.createElement("li");
      li.className = "asc-row";
      li.dataset["testid"] = `wt-${row.wsId}`;
      const path = doc.createElement("span");
      path.className = "asc-subject";
      path.textContent = row.path;
      const head = doc.createElement("span");
      head.className = "asc-dot";
      head.textContent = row.headRef;
      li.append(path, head, badge(doc, "state", summary(row)));
      if (row.current) li.append(badge(doc, "current"));
      if (row.locked !== "") li.append(badge(doc, "locked"));
      if (row.inProgress !== "") li.append(badge(doc, "in-progress", row.inProgress));
      for (const action of ["open", "remove"] as const) {
        const st = actionState(row, action);
        const b = doc.createElement("button");
        b.className = st.enabled ? "asc-action" : "asc-action asc-disabled";
        b.dataset["action"] = action;
        b.textContent = action === "open" ? "Open" : "Remove";
        if (st.enabled) {
          b.addEventListener("click", () => (action === "open" ? host.open(row.path) : host.remove(row.path)));
        } else {
          b.setAttribute("aria-disabled", "true");
          b.title = st.reason ?? "";
        }
        li.append(b);
      }
      list.append(li);
    }
    root.append(list);
  };
  render(initial);
  return {
    update: render,
    dispose() {
      root.remove();
    },
  };
}
