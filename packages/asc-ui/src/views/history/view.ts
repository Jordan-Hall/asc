// Visual file history DOM view. Swim lanes per author, node size by churn. Text is set only
// through `textContent`, so no server string is parsed as markup.
import { type HistoryEntry, layoutHistory } from "./model";

export interface HistoryHost {
  /** Open the file's diff at this revision against its parent. */
  openDiff(path: string, rev: string): void;
  /** Fetch the next page (`next_page_token`); absent when the server has none. */
  loadMore?(): void;
}

export interface HistoryData {
  readonly file: string;
  readonly entries: readonly HistoryEntry[];
  readonly truncated?: boolean;
}

export interface HistoryView {
  update(data: HistoryData): void;
  dispose(): void;
}

const ROW_H = 30;
const LANE_W = 28;

function badge(doc: Document, kind: string, text: string): HTMLElement {
  const b = doc.createElement("span");
  b.className = "asc-badge";
  b.dataset["badge"] = kind;
  b.textContent = text;
  return b;
}

export function mountHistory(
  container: HTMLElement,
  host: HistoryHost,
  initial: HistoryData,
): HistoryView {
  const doc = container.ownerDocument;
  const root = doc.createElement("div");
  root.className = "asc-history";
  root.dataset["testid"] = "history";
  container.append(root);

  const render = (data: HistoryData): void => {
    root.replaceChildren();
    const layout = layoutHistory(data.entries, data.file);
    const head = doc.createElement("div");
    head.className = "asc-toolbar";
    head.textContent = data.file;
    root.append(head);
    if (!layout.hasStats) {
      const note = doc.createElement("div");
      note.className = "asc-note";
      note.dataset["testid"] = "no-stats";
      note.textContent = "Sizes are uniform: the server does not report diffstat.v1";
      root.append(note);
    }
    const laneBar = doc.createElement("div");
    laneBar.className = "asc-lanes";
    layout.lanes.forEach((name, i) => {
      const l = doc.createElement("span");
      l.className = "asc-lane-label";
      l.dataset["lane"] = String(i);
      l.textContent = name;
      laneBar.append(l);
    });
    root.append(laneBar);
    if (layout.nodes.length === 0) {
      const empty = doc.createElement("div");
      empty.className = "asc-note";
      empty.textContent = "No history for this file";
      root.append(empty);
    }
    const list = doc.createElement("ol");
    list.className = "asc-hist-rows";
    for (const node of layout.nodes) {
      const li = doc.createElement("li");
      li.className = "asc-hist-row";
      li.dataset["testid"] = `rev-${node.entry.id}`;
      li.style.height = `${ROW_H}px`;
      li.tabIndex = 0;
      const track = doc.createElement("span");
      track.className = "asc-track";
      track.style.width = `${Math.max(1, layout.lanes.length) * LANE_W}px`;
      const dot = doc.createElement("span");
      dot.className = "asc-node";
      dot.dataset["lane"] = String(node.lane);
      dot.dataset["size"] = String(node.size);
      dot.dataset["churn"] = String(node.churn);
      dot.style.width = `${node.size}px`;
      dot.style.height = `${node.size}px`;
      dot.style.left = `${node.lane * LANE_W + (LANE_W - node.size) / 2}px`;
      track.append(dot);
      const subject = doc.createElement("span");
      subject.className = "asc-subject";
      subject.textContent = node.entry.subject;
      const meta = doc.createElement("span");
      meta.className = "asc-dot";
      meta.textContent = `${node.entry.author} · ${new Date(node.entry.time * 1000).toISOString().slice(0, 10)}`;
      li.append(track, subject, meta);
      if (node.renamedFrom !== undefined)
        li.append(badge(doc, "rename", `renamed from ${node.renamedFrom}`));
      if (node.binary) li.append(badge(doc, "binary", "binary"));
      const open = (): void => host.openDiff(node.path, node.entry.id);
      li.addEventListener("dblclick", open);
      li.addEventListener("keydown", (e) => {
        if (e.key === "Enter") open();
      });
      list.append(li);
    }
    root.append(list);
    if (data.truncated && host.loadMore) {
      const more = doc.createElement("button");
      more.className = "asc-action";
      more.dataset["action"] = "more";
      more.textContent = "Load more";
      more.addEventListener("click", () => host.loadMore?.());
      root.append(more);
    }
  };
  render(initial);
  return {
    update: render,
    dispose() {
      root.remove();
    },
  };
}
