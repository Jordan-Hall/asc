// Compare DOM view: pick two sides (revision, change or stack), show ahead and behind commits
// and the changed files. Text is set only through `textContent`.
import { type DetailFile, shortId, sortFiles, STATUS_LETTER } from "../details/model";
import { type CompareData, type CompareKind, type CompareSide, isEmptySide, sideLabel, summary } from "./model";

export interface CompareHost {
  /** Run the compare for two sides; the caller fetches and calls `update`. */
  compare(a: CompareSide, b: CompareSide): void;
  openDiff(file: DetailFile, data: CompareData): void;
  openAllChanges(data: CompareData): void;
  openRevision(rev: string): void;
  loadMore?(side: "ahead" | "behind"): void;
}

export interface CompareView {
  update(data: CompareData): void;
  dispose(): void;
}

const KINDS: readonly CompareKind[] = ["revision", "change", "stack"];

function el<K extends keyof HTMLElementTagNameMap>(
  doc: Document,
  tag: K,
  cls: string,
  text?: string,
  testid?: string,
): HTMLElementTagNameMap[K] {
  const e = doc.createElement(tag);
  e.className = cls;
  if (text !== undefined) e.textContent = text;
  if (testid !== undefined) e.dataset["testid"] = testid;
  return e;
}

function activate(node: HTMLElement, fn: () => void): void {
  node.tabIndex = 0;
  node.addEventListener("dblclick", fn);
  node.addEventListener("keydown", (e) => {
    if (e.key === "Enter") fn();
  });
}

export function mountCompare(
  container: HTMLElement,
  host: CompareHost,
  initial?: { a: CompareSide; b: CompareSide; data?: CompareData },
): CompareView {
  const doc = container.ownerDocument;
  const root = el(doc, "div", "asc-compare");
  root.dataset["testid"] = "compare";
  container.append(root);

  const picker = el(doc, "div", "asc-meta");
  const side = (name: "a" | "b", init: CompareSide | undefined) => {
    const kind = doc.createElement("select");
    kind.dataset["testid"] = `${name}-kind`;
    for (const k of KINDS) {
      const o = doc.createElement("option");
      o.value = k;
      o.textContent = k;
      kind.append(o);
    }
    kind.value = init?.kind ?? "revision";
    const input = doc.createElement("input");
    input.type = "text";
    input.dataset["testid"] = `${name}-value`;
    input.placeholder = name === "a" ? "base" : "compare";
    input.value = init?.value ?? "";
    return {
      kind,
      input,
      get: (): CompareSide => ({ kind: kind.value as CompareKind, value: input.value.trim() }),
    };
  };
  const sa = side("a", initial?.a);
  const sb = side("b", initial?.b);
  const go = el(doc, "button", "asc-action", "Compare");
  go.dataset["action"] = "compare";
  const run = (): void => {
    const a = sa.get();
    const b = sb.get();
    if (isEmptySide(a) || isEmptySide(b)) {
      status.textContent = "Pick both sides";
      return;
    }
    host.compare(a, b);
  };
  go.addEventListener("click", run);
  for (const i of [sa.input, sb.input]) {
    i.addEventListener("keydown", (e) => {
      if (e.key === "Enter") run();
    });
  }
  picker.append(sa.kind, sa.input, el(doc, "span", "", "..."), sb.kind, sb.input, go);
  const status = el(doc, "div", "asc-note", "", "status");
  const out = el(doc, "div", "asc-compare-out");
  root.append(picker, status, out);

  const commits = (title: string, entries: CompareData["aheadEntries"], more: boolean, which: "ahead" | "behind") => {
    out.append(el(doc, "h3", "", `${title} (${entries.length}${more ? "+" : ""})`, `${which}-head`));
    const ul = el(doc, "ul", "asc-commits");
    for (const e of entries) {
      const li = el(doc, "li", "asc-commit");
      li.dataset["testid"] = `${which}-${e.id}`;
      li.append(el(doc, "span", "asc-id", shortId(e.id)), el(doc, "span", "asc-subject", e.subject));
      li.append(el(doc, "span", "asc-count", e.author));
      activate(li, () => host.openRevision(e.id));
      ul.append(li);
    }
    out.append(ul);
    if (more && host.loadMore) {
      const b = el(doc, "button", "asc-action", "Load more");
      b.dataset["action"] = `more-${which}`;
      b.addEventListener("click", () => host.loadMore?.(which));
      out.append(b);
    }
  };

  const render = (data: CompareData): void => {
    out.replaceChildren();
    status.textContent = `${sideLabel(data.b)} vs ${sideLabel(data.a)}: ${summary(data)}`;
    status.dataset["state"] = "done";
    commits("Ahead", data.aheadEntries, data.aheadMore, "ahead");
    commits("Behind", data.behindEntries, data.behindMore, "behind");
    out.append(el(doc, "h3", "", `${data.files.length} ${data.files.length === 1 ? "file" : "files"} changed`, "files-head"));
    if (data.files.length > 0) {
      const all = el(doc, "button", "asc-action", "Open all changes");
      all.dataset["action"] = "open-all";
      all.addEventListener("click", () => host.openAllChanges(data));
      out.append(all);
    }
    const ul = el(doc, "ul", "asc-files");
    for (const f of sortFiles(data.files)) {
      const li = el(doc, "li", "asc-file");
      li.dataset["testid"] = `file-${f.path}`;
      const st = el(doc, "span", "asc-status", STATUS_LETTER[f.status]);
      st.dataset["status"] = f.status;
      li.append(st, el(doc, "span", "asc-path", f.oldPath !== undefined ? `${f.oldPath} → ${f.path}` : f.path));
      activate(li, () => host.openDiff(f, data));
      ul.append(li);
    }
    out.append(ul);
  };
  if (initial?.data) render(initial.data);
  return {
    update: render,
    dispose() {
      root.remove();
    },
  };
}
