// Browser entry for the Playwright spec only: mounts the details and compare views over a
// recording fake host. Not part of the package exports or the production bundle.
import { applyTokens, resolveTokens } from "../../theme";
import { type CompareData, type CompareSide, mountCompare } from "../compare";
import { smartlogCss } from "../smartlog/style";
import { type DetailsData, detailsCss, mountDetails } from "./index";

interface DetailsCalls {
  diffs: string[];
  all: string[];
  revs: string[];
  reviews: string[];
  compares: { a: CompareSide; b: CompareSide }[];
}

declare global {
  interface Window {
    ascDetailsCalls: DetailsCalls;
    ascDetailsStart(data: { details: DetailsData; compare: CompareData }): void;
  }
}

window.ascDetailsStart = ({ details, compare }) => {
  applyTokens(document.documentElement, resolveTokens());
  const style = document.createElement("style");
  style.textContent = smartlogCss + detailsCss;
  document.head.append(style);
  const calls: DetailsCalls = { diffs: [], all: [], revs: [], reviews: [], compares: [] };
  window.ascDetailsCalls = calls;
  const d = document.createElement("div");
  const c = document.createElement("div");
  document.body.append(d, c);
  mountDetails(d, {
    openDiff: (f) => void calls.diffs.push(f.path),
    openAllChanges: (x) => void calls.all.push(x.id),
    openRevision: (r) => void calls.revs.push(r),
    openReview: (u) => void calls.reviews.push(u),
  }, details);
  const view = mountCompare(c, {
    compare: (a, b) => {
      calls.compares.push({ a, b });
      view.update({ ...compare, a, b });
    },
    openDiff: (f) => void calls.diffs.push(`cmp:${f.path}`),
    openAllChanges: () => void calls.all.push("cmp"),
    openRevision: (r) => void calls.revs.push(`cmp:${r}`),
  });
};
