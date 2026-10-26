// Browser entry for the Playwright specs only: mounts the graph over a generated history served
// by a recording fake host. Not part of the package exports or the production bundle.
import { applyTokens, resolveTokens } from "../../theme";
import {
  type GraphPage,
  type GraphQuery,
  type GraphRow,
  type GraphVerbCall,
  PAGE_SIZE,
  type RefMarker,
} from "./model";
import { genRow } from "./fixture";
import { graphCss } from "./style";
import { type GraphView, mountGraph } from "./view";

export interface StartOptions {
  readonly total: number;
  /** Omit `total` and `ref_markers` from the first page (API addition A2 not served). */
  readonly omitTotal?: boolean;
  readonly delayMs?: number;
}

interface Calls {
  queries: GraphQuery[];
  runs: GraphVerbCall[];
  copied: string[];
  selected: string[];
  opened: string[];
  notes: string[];
  maxFetched: number;
}

declare global {
  interface Window {
    ascGraph: { view: GraphView; calls: Calls; paintMs: number };
    ascStartGraph(opts: StartOptions): void;
    ascBumpHead(): Promise<void>;
  }
}

window.ascStartGraph = (opts) => {
  applyTokens(document.documentElement, resolveTokens());
  const style = document.createElement("style");
  style.textContent = graphCss;
  document.head.append(style);
  document.body.style.margin = "0";
  const host = document.createElement("div");
  host.id = "graph-host";
  host.style.height = "600px";
  host.style.width = "900px";
  document.body.append(host);
  const calls: Calls = {
    queries: [],
    runs: [],
    copied: [],
    selected: [],
    opened: [],
    notes: [],
    maxFetched: 0,
  };
  let total = opts.total;
  let dataAt = -1;
  const markers = (): RefMarker[] => {
    const out: RefMarker[] = [{ name: "main", row: 0 }];
    for (let r = 50; r < total; r += 100) out.push({ name: `tag-${r}`, row: r });
    return out;
  };
  const view = mountGraph(host, {
    async fetchPage(query, signal) {
      calls.queries.push(query);
      if (opts.delayMs) await new Promise((r) => setTimeout(r, opts.delayMs));
      if (signal.aborted) throw new DOMException("aborted", "AbortError");
      const p = query.pageToken === "" ? 0 : Number(query.pageToken);
      const rows: GraphRow[] = [];
      for (let i = p * PAGE_SIZE; i < Math.min(total, (p + 1) * PAGE_SIZE); i++)
        rows.push(genRow(i, total));
      calls.maxFetched = Math.max(calls.maxFetched, p);
      const page: GraphPage = {
        rows,
        ...((p + 1) * PAGE_SIZE < total ? { next: String(p + 1) } : {}),
        ...(p === 0 && !opts.omitTotal ? { total, refMarkers: markers() } : {}),
      };
      if (dataAt < 0) dataAt = performance.now();
      return page;
    },
    async run(call) {
      calls.runs.push(call);
      return { ok: true };
    },
    async copy(text) {
      calls.copied.push(text);
    },
    notify(_s, m) {
      calls.notes.push(m);
    },
    onSelect(row) {
      calls.selected.push(row.id);
    },
    openDetails(row) {
      calls.opened.push(row.id);
    },
  });
  const state = { view, calls, paintMs: -1 };
  window.ascGraph = state;
  // First 200 rows painted: the first row element with content.
  // A MutationObserver, not requestAnimationFrame: a cold page may not schedule a frame for a while.
  const seen = (): boolean => {
    if (dataAt >= 0 && host.querySelector('[data-testid^="grow-"]')) {
      state.paintMs = performance.now() - dataAt;
      return true;
    }
    return false;
  };
  if (!seen()) {
    const mo = new MutationObserver(() => {
      if (seen()) mo.disconnect();
    });
    mo.observe(host, { childList: true, subtree: true });
  }
  window.ascBumpHead = async () => {
    total += 1;
    await view.refreshHead();
  };
};
