// Browser entry for the Playwright spec only: mounts the submit dialog over a recording fake host.
// Not part of the package exports or the production bundle.
import { applyTokens, resolveTokens } from "../../theme";
import type { RunSpec } from "../split/model";
import type { PullEdit, SubmitChange } from "./model";
import { submitCss } from "./style";
import { mountSubmitDialog } from "./view";

declare global {
  interface Window {
    ascSubmit: { runs: RunSpec[]; edits: PullEdit[]; notes: string[] };
    ascStartSubmit(changes: SubmitChange[], assignees: string[], fail?: string): void;
  }
}

window.ascStartSubmit = (changes, assignees, fail) => {
  applyTokens(document.documentElement, resolveTokens());
  const style = document.createElement("style");
  style.textContent = submitCss;
  document.head.append(style);
  const host = document.createElement("div");
  document.body.append(host);
  const rec = { runs: [] as RunSpec[], edits: [] as PullEdit[], notes: [] as string[] };
  mountSubmitDialog(
    host,
    {
      async runSubmit(run) {
        rec.runs.push(run);
        return fail === undefined ? { ok: true } : { ok: false, error: fail };
      },
      async editPull(edit) {
        rec.edits.push(edit);
      },
      async loadAssignees() {
        return assignees;
      },
      notify(_s, m) {
        rec.notes.push(m);
      },
    },
    changes,
    changes[0]?.id,
  );
  window.ascSubmit = rec;
};
