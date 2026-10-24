// Commit details DOM view. Text is set only through `textContent`, so no server string (commit
// message, author, PR title) is parsed as markup; links open through the host and only if https.
import {
  type DetailFile,
  type DetailsData,
  isSigned,
  safeUrl,
  shortId,
  sortFiles,
  splitMessage,
  STATUS_LETTER,
  totals,
} from "./model";

export interface DetailsHost {
  /** Open one file's diff (commit against its first parent). */
  openDiff(file: DetailFile, data: DetailsData): void;
  /** Open every changed file in one multi-diff. */
  openAllChanges(data: DetailsData): void;
  /** Show the details of another revision (a version of the change, a parent). */
  openRevision(rev: string): void;
  /** Open the PR page; only https URLs reach this. */
  openReview(url: string): void;
}

export interface DetailsView {
  update(data: DetailsData): void;
  dispose(): void;
}

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

const date = (secs: number): string => new Date(secs * 1000).toISOString().slice(0, 10);

function activate(node: HTMLElement, fn: () => void): void {
  node.tabIndex = 0;
  node.addEventListener("dblclick", fn);
  node.addEventListener("keydown", (e) => {
    if (e.key === "Enter") fn();
  });
}

export function mountDetails(container: HTMLElement, host: DetailsHost, initial: DetailsData): DetailsView {
  const doc = container.ownerDocument;
  const root = el(doc, "div", "asc-details");
  root.dataset["testid"] = "details";
  container.append(root);

  const render = (data: DetailsData): void => {
    root.replaceChildren();
    const { subject, body } = splitMessage(data.message);
    root.append(el(doc, "h2", "asc-subject", subject, "subject"));
    if (body !== "") root.append(el(doc, "div", "asc-body", body, "body"));

    const meta = el(doc, "div", "asc-meta");
    meta.append(el(doc, "span", "asc-author", `${data.author.name} <${data.author.email}>`, "author"));
    meta.append(el(doc, "span", "asc-date", date(data.author.time)));
    meta.append(el(doc, "span", "asc-id", shortId(data.id), "id"));
    if (isSigned(data.signature)) meta.append(el(doc, "span", "asc-badge", "signed", "signed"));
    if (data.change !== undefined) {
      meta.append(el(doc, "span", "asc-badge", `change ${shortId(data.change)}`, "change"));
    }
    root.append(meta);

    if (data.review) {
      const r = data.review;
      const url = safeUrl(r.url);
      const label = `${r.forge} #${r.number} ${r.draft ? "draft" : r.state}`;
      const row = el(doc, "div", "asc-meta", undefined, "review");
      if (url !== undefined) {
        const b = el(doc, "button", "asc-link", label);
        b.dataset["action"] = "open-review";
        b.addEventListener("click", () => host.openReview(url));
        row.append(b);
      } else {
        row.append(el(doc, "span", "asc-badge", label));
      }
      root.append(row);
    }

    if (data.parents.length > 0) {
      const row = el(doc, "div", "asc-meta");
      row.append(el(doc, "span", "", data.parents.length > 1 ? "Parents" : "Parent"));
      for (const p of data.parents) {
        const b = el(doc, "button", "asc-link", shortId(p));
        b.dataset["action"] = "parent";
        b.addEventListener("click", () => host.openRevision(p));
        row.append(b);
      }
      root.append(row);
    }

    const t = totals(data.files);
    const head = el(doc, "h3", "", `${t.files} ${t.files === 1 ? "file" : "files"} changed`, "files-head");
    if (t.known) head.textContent += `, +${t.added} -${t.removed}`;
    root.append(head);
    if (data.files.length > 0) {
      const all = el(doc, "button", "asc-action", "Open all changes");
      all.dataset["action"] = "open-all";
      all.addEventListener("click", () => host.openAllChanges(data));
      root.append(all);
    }
    const list = el(doc, "ul", "asc-files");
    for (const f of sortFiles(data.files)) {
      const li = el(doc, "li", "asc-file");
      li.dataset["testid"] = `file-${f.path}`;
      const st = el(doc, "span", "asc-status", STATUS_LETTER[f.status]);
      st.dataset["status"] = f.status;
      li.append(st, el(doc, "span", "asc-path", f.oldPath !== undefined ? `${f.oldPath} → ${f.path}` : f.path));
      if (f.binary === true) li.append(el(doc, "span", "asc-badge", "binary"));
      else if (f.added !== undefined) li.append(el(doc, "span", "asc-count", `+${f.added} -${f.removed ?? 0}`));
      activate(li, () => host.openDiff(f, data));
      list.append(li);
    }
    root.append(list);
    if (data.truncated === true) root.append(el(doc, "div", "asc-note", "The file list is truncated", "truncated"));

    if (data.versions.length > 0) {
      root.append(el(doc, "h3", "", "Versions"));
      const vl = el(doc, "ul", "asc-versions");
      for (const v of data.versions) {
        const li = el(doc, "li", "asc-version", `v${v.v} ${shortId(v.rev)} ${v.actor} ${date(v.at)}`);
        li.dataset["testid"] = `version-${v.v}`;
        activate(li, () => host.openRevision(v.rev));
        vl.append(li);
      }
      root.append(vl);
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
