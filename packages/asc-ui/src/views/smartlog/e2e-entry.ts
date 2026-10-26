// Browser entry for the Playwright spec only: mounts the smartlog over a recording fake host.
// Not part of the package exports or the production bundle.
import { applyTokens, resolveTokens } from "../../theme";
import type { MoveTarget } from "../../ops/mutations";
import type { MovePrediction, VerbCall } from "./model";
import { smartlogCss } from "./style";
import { mountSmartlog, type SmartlogData, type SmartlogView } from "./view";

interface Calls {
  moves: { change: string; onto: MoveTarget }[];
  predictions: { change: string; onto: MoveTarget }[];
  runs: VerbCall[];
  notes: string[];
}

declare global {
  interface Window {
    ascSmartlog: { calls: Calls; view: SmartlogView };
    ascStart(data: SmartlogData, conflicts: number): void;
  }
}

window.ascStart = (data, conflicts) => {
  applyTokens(document.documentElement, resolveTokens());
  const style = document.createElement("style");
  style.textContent = smartlogCss;
  document.head.append(style);
  const host = document.createElement("div");
  host.id = "smartlog-host";
  document.body.append(host);
  const calls: Calls = { moves: [], predictions: [], runs: [], notes: [] };
  const view = mountSmartlog(
    host,
    {
      enqueueMove(change, onto) {
        calls.moves.push({ change, onto });
        return {
          ok: true,
          op: { id: 1, mutation: { kind: "move", change, onto }, label: "move", status: "queued" },
        };
      },
      async predictMove(change, onto): Promise<MovePrediction> {
        calls.predictions.push({ change, onto });
        return { ok: true, conflicts };
      },
      async run(call) {
        calls.runs.push(call);
        return { ok: true };
      },
      notify(_s, m) {
        calls.notes.push(m);
      },
    },
    data,
  );
  window.ascSmartlog = { calls, view };
};
