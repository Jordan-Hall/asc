// Browser entry for the Playwright spec only: mounts the history and worktrees views over a
// recording fake host. Not part of the package exports or the production bundle.
import { applyTokens, resolveTokens } from "../../theme";
import { smartlogCss } from "../smartlog/style";
import type { WorktreeRow } from "../worktrees/model";
import { mountWorktrees } from "../worktrees/view";
import type { HistoryEntry } from "./model";
import { historyCss } from "./style";
import { mountHistory } from "./view";

interface Calls {
  diffs: { path: string; rev: string }[];
  opened: string[];
  removed: string[];
  added: number;
}

declare global {
  interface Window {
    ascCalls: Calls;
    ascHistoryStart(data: { file: string; entries: HistoryEntry[]; worktrees: WorktreeRow[] }): void;
  }
}

window.ascHistoryStart = (data) => {
  applyTokens(document.documentElement, resolveTokens());
  const style = document.createElement("style");
  style.textContent = smartlogCss + historyCss;
  document.head.append(style);
  const calls: Calls = { diffs: [], opened: [], removed: [], added: 0 };
  window.ascCalls = calls;
  const h = document.createElement("div");
  const w = document.createElement("div");
  document.body.append(h, w);
  mountHistory(h, { openDiff: (path, rev) => void calls.diffs.push({ path, rev }) }, data);
  mountWorktrees(
    w,
    {
      open: (p) => void calls.opened.push(p),
      remove: (p) => void calls.removed.push(p),
      add: () => void calls.added++,
    },
    data.worktrees,
  );
};
