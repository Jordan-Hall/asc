// Search DOM view (SPEC §24.4.7). Vanilla DOM; every server string goes through `textContent`.
// A new submit aborts the one in flight and stale batches are ignored.
import type { Severity } from "../../platform";
import {
  appendCapped,
  coverageSummary,
  type Features,
  highlightRuns,
  type IndexCoverage,
  MODES,
  modeAvailability,
  queryBlockedReason,
  type ResultItem,
  resultCountText,
  SCOPES,
  type ScopeKind,
  scopeAvailability,
  scopeNeedsValue,
  type SearchMode,
  type SearchQuery,
} from "./model";

export interface SearchBatch {
  readonly items: readonly ResultItem[];
  /** The server stopped early (budget); more results exist. */
  readonly truncated?: boolean;
}

export interface SearchHost {
  /** Streams results. Must stop when `signal` aborts. Routes to CodeIntel or Repo.Grep itself. */
  search(query: SearchQuery, signal: AbortSignal): AsyncIterable<SearchBatch>;
  /** `CodeIntel.IndexStatus` for the scope; only called when search.v1 is present. */
  indexStatus(scope: SearchQuery["scope"], signal: AbortSignal): Promise<readonly IndexCoverage[]>;
  open(item: ResultItem, query: SearchQuery): void;
  notify(severity: Severity, message: string): void;
}

export interface SearchView {
  setFeatures(f: Features): void;
  /** Prefill and run (the quick pick's "open in panel"). */
  run(query: Partial<SearchQuery>): void;
  dispose(): void;
}

const MODE_LABEL: Record<SearchMode, string> = {
  exact: "Exact",
  regex: "Regex",
  symbol: "Symbol",
  semantic: "Semantic",
};
const SCOPE_LABEL: Record<ScopeKind, string> = {
  workspace: "Workspace",
  revision: "Revision",
  change: "Change",
  stack: "Stack",
};

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

export function mountSearch(
  container: HTMLElement,
  host: SearchHost,
  features: Features,
  initial?: Partial<SearchQuery>,
): SearchView {
  const doc = container.ownerDocument;
  let feats = features;
  let mode: SearchMode = initial?.mode ?? "exact";
  let scopeKind: ScopeKind = initial?.scope?.kind ?? "workspace";
  let scopeValue = initial?.scope?.value ?? "";
  let caseSensitive = initial?.caseSensitive ?? false;
  let text = initial?.text ?? "";
  let items: ResultItem[] = [];
  let truncated = false;
  let status = "";
  let coverage: readonly IndexCoverage[] | undefined;
  let abort: AbortController | undefined;
  let seq = 0;
  let disposed = false;

  const query = (): SearchQuery => ({
    mode,
    text,
    scope: { kind: scopeKind, value: scopeValue },
    caseSensitive,
  });

  const root = el(doc, "div", "asc-search");
  container.replaceChildren(root);

  function render(): void {
    if (disposed) return;
    if (!modeAvailability(mode, feats).enabled) mode = "exact";
    if (!scopeAvailability(scopeKind, feats).enabled) scopeKind = "workspace";

    const form = el(doc, "form", "asc-search-form");
    form.addEventListener("submit", (ev) => {
      ev.preventDefault();
      submit();
    });

    const modes = el(doc, "div", "asc-search-modes");
    modes.setAttribute("role", "group");
    modes.setAttribute("aria-label", "Search mode");
    for (const m of MODES) {
      const av = modeAvailability(m, feats);
      const b = el(doc, "button", "asc-mode", MODE_LABEL[m]);
      b.type = "button";
      b.dataset.mode = m;
      b.setAttribute("aria-pressed", String(m === mode));
      b.setAttribute("aria-disabled", String(!av.enabled));
      if (av.enabled) {
        b.addEventListener("click", () => {
          mode = m;
          render();
        });
      } else {
        // aria-disabled keeps the button focusable so the reason is reachable by keyboard.
        b.classList.add("asc-disabled");
        b.title = av.reason ?? "";
        b.dataset.reason = av.reason ?? "";
      }
      modes.append(b);
    }

    const go = el(doc, "button", "asc-action", "Search");
    go.type = "submit";
    go.dataset.testid = "search-go";
    const updateSubmit = (): void => {
      const why = queryBlockedReason(query(), feats);
      go.setAttribute("aria-disabled", String(why !== undefined));
      go.classList.toggle("asc-disabled", why !== undefined);
      go.title = why ?? "";
    };

    const box = el(doc, "input", "asc-search-input");
    box.type = "search";
    box.value = text;
    box.placeholder = mode === "semantic" ? "Describe what you are looking for" : "Search";
    box.setAttribute("aria-label", "Search text");
    box.dataset.testid = "search-input";
    box.addEventListener("input", () => {
      text = box.value;
      updateSubmit();
    });

    const scope = el(doc, "select", "asc-scope");
    scope.setAttribute("aria-label", "Scope");
    scope.dataset.testid = "scope";
    for (const s of SCOPES) {
      const av = scopeAvailability(s, feats);
      const o = el(doc, "option", undefined, SCOPE_LABEL[s]);
      o.value = s;
      o.selected = s === scopeKind;
      o.disabled = !av.enabled;
      if (!av.enabled) o.title = av.reason ?? "";
      scope.append(o);
    }
    scope.addEventListener("change", () => {
      scopeKind = scope.value as ScopeKind;
      render();
    });

    const value = el(doc, "input", "asc-scope-value");
    value.type = "text";
    value.hidden = !scopeNeedsValue(scopeKind);
    value.value = scopeValue;
    value.placeholder = scopeKind === "revision" ? "revision" : "change id";
    value.setAttribute("aria-label", "Scope value");
    value.dataset.testid = "scope-value";
    value.addEventListener("input", () => {
      scopeValue = value.value;
      updateSubmit();
    });

    const cs = el(doc, "label", "asc-case");
    const csBox = el(doc, "input");
    csBox.type = "checkbox";
    csBox.checked = caseSensitive;
    csBox.addEventListener("change", () => {
      caseSensitive = csBox.checked;
    });
    cs.append(csBox, doc.createTextNode(" Match case"));
    updateSubmit();

    form.append(modes, box, scope, value, cs, go);

    const cov = coverageSummary(coverage, feats);
    const covEl = el(
      doc,
      "div",
      cov.partial ? "asc-coverage asc-partial" : "asc-coverage",
      cov.text,
    );
    covEl.dataset.testid = "coverage";

    const statusEl = el(doc, "div", "asc-status", status);
    statusEl.dataset.testid = "status";
    statusEl.setAttribute("role", "status");

    const list = el(doc, "ul", "asc-results");
    list.dataset.testid = "results";
    for (const it of items) list.append(resultRow(it));

    root.replaceChildren(form, covEl, statusEl, list);
  }

  function resultRow(it: ResultItem): HTMLLIElement {
    const li = el(doc, "li", "asc-hit");
    li.tabIndex = 0;
    li.dataset.path = it.path;
    li.dataset.line = String(it.line);
    const loc = el(doc, "span", "asc-hit-loc", it.line > 0 ? `${it.path}:${it.line}` : it.path);
    const body = el(doc, "span", "asc-hit-text");
    if (it.redacted) {
      body.textContent = "Content hidden by policy";
      body.classList.add("asc-redacted");
    } else if (it.label !== undefined) {
      body.textContent = it.label;
    } else {
      for (const run of highlightRuns(it.text, it.cols, it.length)) {
        if (run.hit) body.append(el(doc, "mark", undefined, run.text));
        else body.append(doc.createTextNode(run.text));
      }
    }
    li.append(loc, body);
    const open = (): void => host.open(it, query());
    li.addEventListener("click", open);
    li.addEventListener("keydown", (ev) => {
      if (ev.key === "Enter") open();
    });
    return li;
  }

  function submit(): void {
    const q = query();
    const why = queryBlockedReason(q, feats);
    if (why !== undefined) {
      status = why;
      render();
      return;
    }
    abort?.abort();
    const ctl = new AbortController();
    abort = ctl;
    const mine = ++seq;
    items = [];
    truncated = false;
    status = "Searching";
    render();
    if (feats.searchV1) {
      host
        .indexStatus(q.scope, ctl.signal)
        .then((c) => {
          if (mine !== seq || disposed) return;
          coverage = c;
          render();
        })
        .catch(() => {
          // Coverage is advisory; the search itself reports real failures.
        });
    }
    void (async () => {
      try {
        for await (const batch of host.search(q, ctl.signal)) {
          if (mine !== seq || disposed) return;
          const r = appendCapped(items, batch.items);
          items = r.items;
          truncated = truncated || r.capped || batch.truncated === true;
          render();
          if (r.capped) {
            ctl.abort();
            break;
          }
        }
        if (mine !== seq || disposed) return;
        status = resultCountText(items.length, truncated);
      } catch (e) {
        if (mine !== seq || disposed || ctl.signal.aborted) return;
        status = "Search failed";
        host.notify("error", e instanceof Error ? e.message : "Search failed");
      }
      render();
    })();
  }

  render();
  if (initial?.text) submit();

  return {
    setFeatures(f) {
      feats = f;
      render();
    },
    run(q) {
      if (q.mode) mode = q.mode;
      if (q.text !== undefined) text = q.text;
      if (q.scope) {
        scopeKind = q.scope.kind;
        scopeValue = q.scope.value;
      }
      if (q.caseSensitive !== undefined) caseSensitive = q.caseSensitive;
      render();
      submit();
    },
    dispose() {
      disposed = true;
      abort?.abort();
      root.remove();
    },
  };
}
