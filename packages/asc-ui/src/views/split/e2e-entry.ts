// Browser entry for the Playwright spec only: mounts the split dialog over a recording fake host.
// Not part of the package exports or the production bundle.
import { applyTokens, resolveTokens } from "../../theme";
import type { SplitSpec } from "./model";
import { splitCss } from "./style";
import { mountSplitDialog } from "./view";

declare global {
  interface Window {
    ascSplit: { specs: SplitSpec[]; notes: string[] };
    ascStartSplit(diff: string, truncated: boolean, fail?: string): void;
  }
}

window.ascStartSplit = (diff, truncated, fail) => {
  applyTokens(document.documentElement, resolveTokens());
  const style = document.createElement("style");
  style.textContent = splitCss;
  document.head.append(style);
  const host = document.createElement("div");
  document.body.append(host);
  const rec = { specs: [] as SplitSpec[], notes: [] as string[] };
  mountSplitDialog(
    host,
    {
      async applySplit(_change, spec) {
        rec.specs.push(spec);
        return fail === undefined ? { ok: true } : { ok: false, error: fail };
      },
      notify(_s, m) {
        rec.notes.push(m);
      },
    },
    { change: "aaaa1111", subject: "Original subject", diff, truncated },
  );
  window.ascSplit = rec;
};
