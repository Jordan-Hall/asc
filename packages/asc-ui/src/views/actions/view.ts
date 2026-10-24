// Actions view: runs list, jobs of the selected run, log tail of the selected job. Vanilla DOM,
// server text only through `textContent`.
import type { Severity } from "../../platform";
import { type JobRow, type RunRow, rerunDisabledReason, runTone, tailLog } from "./model";

export interface ActionsHost {
  listRuns(signal: AbortSignal): Promise<readonly RunRow[]>;
  listJobs(runId: number, signal: AbortSignal): Promise<readonly JobRow[]>;
  jobLog(jobId: number, signal: AbortSignal): Promise<string>;
  rerun(runId: number): Promise<void>;
  /** Open the log as a read-only document (`asc-actions:`) where the host supports it. */
  openLog?(jobId: number): void;
  notify(severity: Severity, message: string): void;
}

export interface ActionsView {
  refresh(): Promise<void>;
  dispose(): void;
}

export function mountActions(container: HTMLElement, host: ActionsHost): ActionsView {
  const doc = container.ownerDocument;
  const root = doc.createElement("div");
  root.className = "asc-actions";
  root.setAttribute("data-testid", "actions");
  const runsEl = doc.createElement("ul");
  runsEl.className = "asc-rows";
  const jobsEl = doc.createElement("ul");
  jobsEl.className = "asc-rows";
  const logEl = doc.createElement("pre");
  logEl.className = "asc-log";
  logEl.setAttribute("data-testid", "log");
  const status = doc.createElement("div");
  status.className = "asc-status";
  root.append(status, runsEl, jobsEl, logEl);
  container.append(root);

  const ctl = new AbortController();
  let gen = 0;
  let selectedRun: number | undefined;

  const fail = (e: unknown): void => {
    if (ctl.signal.aborted) return;
    const m = e instanceof Error ? e.message : "request failed";
    status.textContent = m;
    host.notify("error", m);
  };

  const button = (label: string, action: string, reason: string | undefined, fn: () => void) => {
    const b = doc.createElement("button");
    b.className = reason ? "asc-action asc-disabled" : "asc-action";
    b.textContent = label;
    b.dataset.action = action;
    if (reason) {
      b.title = reason;
      b.setAttribute("aria-disabled", "true");
      b.addEventListener("click", (ev) => ev.stopPropagation());
    } else {
      b.addEventListener("click", (ev) => {
        ev.stopPropagation();
        fn();
      });
    }
    return b;
  };

  const badge = (statusText: string) => {
    const b = doc.createElement("span");
    b.className = `asc-badge asc-tone-${runTone(statusText)}`;
    b.dataset.badge = "status";
    b.textContent = statusText;
    return b;
  };

  async function showLog(jobId: number): Promise<void> {
    const my = ++gen;
    logEl.textContent = "Loading...";
    try {
      const { text, truncated } = tailLog(await host.jobLog(jobId, ctl.signal));
      if (my !== gen) return;
      logEl.textContent = (truncated ? "... (earlier output not shown)\n" : "") + text;
    } catch (e) {
      if (my === gen) {
        logEl.textContent = "";
        fail(e);
      }
    }
  }

  async function showJobs(runId: number): Promise<void> {
    selectedRun = runId;
    gen++;
    logEl.textContent = "";
    jobsEl.replaceChildren();
    try {
      const jobs = await host.listJobs(runId, ctl.signal);
      if (selectedRun !== runId) return;
      for (const j of jobs) {
        const li = doc.createElement("li");
        li.className = "asc-row";
        li.setAttribute("data-testid", `job-${j.id}`);
        li.tabIndex = 0;
        const name = doc.createElement("span");
        name.className = "asc-subject";
        name.textContent = j.name;
        li.append(name, badge(j.status));
        const open = host.openLog;
        if (open) li.append(button("Open log", "open-log", undefined, () => open(j.id)));
        li.addEventListener("click", () => void showLog(j.id));
        jobsEl.append(li);
      }
    } catch (e) {
      fail(e);
    }
  }

  async function refresh(): Promise<void> {
    status.textContent = "";
    try {
      const runs = await host.listRuns(ctl.signal);
      runsEl.replaceChildren();
      if (runs.length === 0) status.textContent = "No runs";
      for (const r of runs) {
        const li = doc.createElement("li");
        li.className = r.id === selectedRun ? "asc-row asc-selected" : "asc-row";
        li.setAttribute("data-testid", `run-${r.id}`);
        li.tabIndex = 0;
        const title = doc.createElement("span");
        title.className = "asc-subject";
        title.textContent = r.title;
        li.append(title, badge(r.status));
        if (r.commit) {
          const c = doc.createElement("span");
          c.className = "asc-dot";
          c.textContent = r.commit.slice(0, 8);
          li.append(c);
        }
        li.append(
          button("Re-run", "rerun", rerunDisabledReason(r.status), () => {
            host.rerun(r.id).then(() => refresh(), fail);
          }),
        );
        li.addEventListener("click", () => void showJobs(r.id));
        runsEl.append(li);
      }
    } catch (e) {
      fail(e);
    }
  }

  return {
    refresh,
    dispose() {
      ctl.abort();
      root.remove();
    },
  };
}
