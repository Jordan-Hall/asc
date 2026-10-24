// Review panel DOM view (SPEC §24.4.7). Vanilla DOM, server text set only through `textContent`.
// Merge and review submit ask the host for its modal confirmation first (never `window.confirm`).
import type { RunOutcome } from "../../ops/queue";
import type { Severity } from "../../platform";
import { locate } from "../../state/model";
import {
  approveConfirmText,
  approveState,
  buildNavigator,
  defaultPick,
  type DiffFile,
  fileSummary,
  groupComments,
  mergeConfirmText,
  mergeState,
  pickError,
  type ReviewData,
  sinceLastReview,
  type VersionPick,
} from "./model";

export type InterDiffOutcome =
  | { readonly ok: true; readonly files: readonly DiffFile[] }
  | { readonly ok: false; readonly error: string };

export interface ReviewHost {
  /** The user picked a change in the navigator; the host loads its data and calls `update`. */
  select(change: string): void;
  /** `Changes.InterDiff` summarised per file (from === to shows that version's own diff). */
  interdiff(change: string, pick: VersionPick, signal: AbortSignal): Promise<InterDiffOutcome>;
  /** Open one file of the change in a native diff editor at the picked versions. */
  openDiff(change: string, path: string, pick: VersionPick): void;
  /** The host's modal confirmation (IP-D1). Resolves false when dismissed. */
  confirm(message: string): Promise<boolean>;
  approve(change: string): Promise<RunOutcome>;
  merge(change: string): Promise<RunOutcome>;
  notify(severity: Severity, message: string): void;
}

export interface ReviewView {
  update(data: ReviewData): void;
  dispose(): void;
}

const el = <K extends keyof HTMLElementTagNameMap>(
  doc: Document,
  tag: K,
  cls?: string,
  text?: string,
): HTMLElementTagNameMap[K] => {
  const e = doc.createElement(tag);
  if (cls) e.className = cls;
  if (text !== undefined) e.textContent = text;
  return e;
};

export function mountReview(
  container: HTMLElement,
  host: ReviewHost,
  initial: ReviewData,
): ReviewView {
  const doc = container.ownerDocument;
  let data = initial;
  let selected = initial.selected;
  let pick: VersionPick | undefined;
  let pickFor: string | undefined;
  let files: readonly DiffFile[] = [];
  let diffError: string | undefined;
  let inflight: AbortController | undefined;
  let busy = false;
  let disposed = false;

  const root = el(doc, "section", "asc-review");
  container.append(root);

  const button = (
    action: string,
    text: string,
    state: { enabled: boolean; reason?: string | undefined },
    onClick: () => void,
  ): HTMLButtonElement => {
    const b = el(doc, "button", state.enabled ? "asc-action" : "asc-action asc-disabled", text);
    b.type = "button";
    b.dataset.action = action;
    if (!state.enabled) {
      b.setAttribute("aria-disabled", "true");
      b.title = state.reason ?? "";
    }
    b.addEventListener("click", () => {
      if (state.enabled && !busy) onClick();
    });
    return b;
  };

  function loadDiff(change: string): void {
    inflight?.abort();
    const current = pick;
    if (!current) return;
    const ac = new AbortController();
    inflight = ac;
    void host.interdiff(change, current, ac.signal).then(
      (r) => {
        if (ac.signal.aborted || disposed) return;
        files = r.ok ? r.files : [];
        diffError = r.ok ? undefined : r.error;
        render();
      },
      (e: unknown) => {
        if (ac.signal.aborted || disposed) return;
        files = [];
        diffError = e instanceof Error ? e.message : "interdiff failed";
        render();
      },
    );
  }

  function setPick(change: string, next: VersionPick): void {
    pick = next;
    pickFor = change;
    files = [];
    diffError = undefined;
    loadDiff(change);
    render();
  }

  async function submit(
    change: string,
    confirmText: string,
    run: () => Promise<RunOutcome>,
    done: string,
  ): Promise<void> {
    busy = true;
    try {
      if (!(await host.confirm(confirmText))) return;
      const out = await run();
      host.notify(out.ok ? "info" : "error", out.ok ? done : out.error);
      if (out.ok) host.select(change);
    } finally {
      busy = false;
    }
  }

  function render(): void {
    const nav = buildNavigator({ ...data, ...(selected === undefined ? {} : { selected }) });
    const at = selected === undefined ? undefined : locate(data.stacks, selected);
    const cd = selected === undefined ? undefined : data.changes[selected];
    const versions = cd?.versions ?? [];
    if (selected !== undefined && cd && pickFor !== selected && versions.length > 0) {
      pick = defaultPick(versions);
      pickFor = selected;
      loadDiff(selected);
    }

    const children: HTMLElement[] = [];

    children.push(el(doc, "h3", undefined, "Stack"));
    const list = el(doc, "ul", "asc-nav");
    for (const row of nav) {
      const li = el(doc, "li", row.selected ? "asc-nav-row asc-selected" : "asc-nav-row");
      li.setAttribute("data-testid", `nav-${row.id}`);
      li.tabIndex = 0;
      li.append(el(doc, "span", "asc-subject", row.subject));
      for (const b of row.badges) {
        const be = el(doc, "span", `asc-badge asc-${b.kind}${b.tone ? ` asc-${b.tone}` : ""}`, b.text);
        be.dataset.badge = b.kind;
        if (b.title) be.title = b.title;
        li.append(be);
      }
      const choose = (): void => {
        selected = row.id;
        host.select(row.id);
        render();
      };
      li.addEventListener("click", choose);
      li.addEventListener("keydown", (e) => {
        if (e.key === "Enter" || e.key === " ") choose();
      });
      list.append(li);
    }
    children.push(list);

    if (at && cd) {
      children.push(el(doc, "h3", undefined, "Versions"));
      const picker = el(doc, "div", "asc-picker");
      const select = (name: "from" | "to"): HTMLSelectElement => {
        const s = el(doc, "select");
        s.setAttribute("aria-label", name === "from" ? "From version" : "To version");
        s.setAttribute("data-testid", `version-${name}`);
        for (const v of versions) {
          const o = el(doc, "option", undefined, `v${v.v} (${v.actor})`);
          o.value = String(v.v);
          s.append(o);
        }
        if (pick) s.value = String(pick[name]);
        s.addEventListener("change", () => {
          const n = Number(s.value);
          if (pick && selected) setPick(selected, { ...pick, [name]: n });
        });
        return s;
      };
      picker.append(select("from"), select("to"));
      const since = sinceLastReview(cd);
      picker.append(
        button(
          "since-last-review",
          "Since my last review",
          "pick" in since ? { enabled: true } : { enabled: false, reason: since.reason },
          () => {
            if ("pick" in since && selected) setPick(selected, since.pick);
          },
        ),
      );
      children.push(picker);

      const err = pick ? pickError(versions, pick) : undefined;
      if (err) {
        const e = el(doc, "div", "asc-error", err);
        e.setAttribute("role", "alert");
        children.push(e);
      } else if (diffError) {
        const e = el(doc, "div", "asc-error", diffError);
        e.setAttribute("role", "alert");
        children.push(e);
      }

      children.push(el(doc, "h3", undefined, "Files"));
      const fl = el(doc, "ul", "asc-files");
      for (const f of files) {
        const li = el(doc, "li", "asc-file", fileSummary(f));
        li.setAttribute("data-testid", `file-${f.path}`);
        li.tabIndex = 0;
        const open = (): void => {
          if (selected && pick) host.openDiff(selected, f.path, pick);
        };
        li.addEventListener("click", open);
        li.addEventListener("keydown", (e) => {
          if (e.key === "Enter") open();
        });
        fl.append(li);
      }
      children.push(fl);

      children.push(el(doc, "h3", undefined, "Comments"));
      const threads = el(doc, "ul", "asc-threads");
      for (const g of groupComments(data.comments[at.change.id] ?? [])) {
        for (const c of g.comments) {
          const li = el(doc, "li", c.resolved ? "asc-thread asc-resolved" : "asc-thread");
          li.setAttribute("data-testid", `thread-${c.id}`);
          li.textContent = `${g.path}${c.line === undefined ? "" : `:${c.line}`} ${c.author}: ${c.body}${
            c.resolved ? " (resolved)" : ""
          }${c.outdated ? " (outdated)" : ""}`;
          threads.append(li);
        }
      }
      children.push(threads);

      const actions = el(doc, "div", "asc-picker");
      const change = at.change;
      actions.append(
        button("approve", "Approve", approveState(change, cd), () => {
          void submit(change.id, approveConfirmText(change), () => host.approve(change.id), "Review submitted");
        }),
        button("merge", "Merge", mergeState(data.stacks, change.id, data.changes), () => {
          void submit(change.id, mergeConfirmText(change), () => host.merge(change.id), "Merged");
        }),
      );
      children.push(actions);
    }

    root.replaceChildren(...children);
  }

  render();
  return {
    update(next) {
      data = next;
      if (next.selected !== undefined) selected = next.selected;
      // Keep the pick while both versions still exist; a new push re-picks the default.
      const vs = selected === undefined ? [] : (next.changes[selected]?.versions ?? []);
      if (!pick || pickError(vs, pick) !== undefined) pickFor = undefined;
      render();
    },
    dispose() {
      disposed = true;
      inflight?.abort();
      root.remove();
    },
  };
}
