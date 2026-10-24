// Browser entry for the Playwright spec only: mounts the search view over a recording fake host.
// Not part of the package exports or the production bundle.
import { applyTokens, resolveTokens } from "../../theme";
import type { IndexCoverage, ResultItem, SearchQuery } from "./index";
import { mountSearch, searchCss, type SearchView } from "./index";

interface Calls {
  searches: SearchQuery[];
  opened: { path: string; line: number }[];
  notes: string[];
  aborted: number;
}

declare global {
  interface Window {
    ascSearch: { calls: Calls; view: SearchView };
    ascStartSearch(opts: {
      searchV1: boolean;
      items: ResultItem[];
      coverage: IndexCoverage[];
      truncated?: boolean;
    }): void;
  }
}

window.ascStartSearch = (opts) => {
  applyTokens(document.documentElement, resolveTokens());
  const style = document.createElement("style");
  style.textContent = searchCss;
  document.head.append(style);
  const host = document.createElement("div");
  host.id = "search-host";
  document.body.append(host);
  const calls: Calls = { searches: [], opened: [], notes: [], aborted: 0 };
  const view = mountSearch(
    host,
    {
      async *search(query, signal) {
        calls.searches.push(query);
        signal.addEventListener("abort", () => {
          calls.aborted++;
        });
        yield { items: opts.items, truncated: opts.truncated === true };
      },
      async indexStatus() {
        return opts.coverage;
      },
      open(item) {
        calls.opened.push({ path: item.path, line: item.line });
      },
      notify(_s, m) {
        calls.notes.push(m);
      },
    },
    { searchV1: opts.searchV1 },
  );
  window.ascSearch = { calls, view };
};
