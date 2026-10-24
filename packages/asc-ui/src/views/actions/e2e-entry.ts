// Browser entry for the Playwright spec only: mounts the CI views over recording fake hosts.
import { applyTokens, resolveTokens } from "../../theme";
import { mountAffected } from "../affected";
import { mountProjects } from "../projects";
import { ciCss } from "./style";
import { mountActions } from "./view";

interface CiData {
  runs: unknown[];
  jobs: Record<number, unknown[]>;
  logs: Record<number, string>;
  affected: unknown[];
  projects: unknown[];
}

declare global {
  interface Window {
    ascCi: { calls: { reruns: number[]; targets: string[][]; opened: string[] } };
    ascStartCi(data: CiData): void;
  }
}

window.ascStartCi = (d) => {
  applyTokens(document.documentElement, resolveTokens());
  const style = document.createElement("style");
  style.textContent = ciCss;
  document.head.append(style);
  const mk = (id: string) => {
    const h = document.createElement("div");
    h.id = id;
    document.body.append(h);
    return h;
  };
  const calls = { reruns: [] as number[], targets: [] as string[][], opened: [] as string[] };
  const notify = () => {};
  const actions = mountActions(mk("actions-host"), {
    listRuns: async () => d.runs as never,
    listJobs: async (id) => (d.jobs[id] ?? []) as never,
    jobLog: async (id) => d.logs[id] ?? "",
    rerun: async (id) => {
      calls.reruns.push(id);
    },
    notify,
  });
  void actions.refresh();
  mountAffected(mk("affected-host"), {
    load: async () => d.affected as never,
    runTarget: (_r, _t, argv) => calls.targets.push([...argv]),
    notify,
  });
  mountProjects(mk("projects-host"), d.projects as never, (id) => calls.opened.push(id));
  window.ascCi = { calls };
};
