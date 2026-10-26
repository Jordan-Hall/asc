// Browser entry for the Playwright spec only: mounts the review panel over a recording fake host.
// Not part of the package exports or the production bundle.
import { applyTokens, resolveTokens } from "../../theme";
import type { DiffFile, ReviewData, VersionPick } from "./model";
import { reviewCss } from "./style";
import { mountReview, type ReviewView } from "./view";

interface Calls {
  selects: string[];
  interdiffs: { change: string; pick: VersionPick }[];
  opened: { change: string; path: string; pick: VersionPick }[];
  confirms: string[];
  approves: string[];
  merges: string[];
  notes: string[];
}

declare global {
  interface Window {
    ascReview: { calls: Calls; view: ReviewView };
    ascStartReview(data: ReviewData, files: DiffFile[], confirm: boolean): void;
  }
}

window.ascStartReview = (data, files, confirm) => {
  applyTokens(document.documentElement, resolveTokens());
  const style = document.createElement("style");
  style.textContent = reviewCss;
  document.head.append(style);
  const host = document.createElement("div");
  document.body.append(host);
  const calls: Calls = {
    selects: [],
    interdiffs: [],
    opened: [],
    confirms: [],
    approves: [],
    merges: [],
    notes: [],
  };
  const view = mountReview(
    host,
    {
      select(change) {
        calls.selects.push(change);
      },
      async interdiff(change, pick) {
        calls.interdiffs.push({ change, pick });
        return { ok: true, files };
      },
      openDiff(change, path, pick) {
        calls.opened.push({ change, path, pick });
      },
      async confirm(message) {
        calls.confirms.push(message);
        return confirm;
      },
      async approve(change) {
        calls.approves.push(change);
        return { ok: true };
      },
      async merge(change) {
        calls.merges.push(change);
        return { ok: true };
      },
      notify(_s, m) {
        calls.notes.push(m);
      },
    },
    data,
  );
  window.ascReview = { calls, view };
};
