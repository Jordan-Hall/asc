// The lists behind the navigation: your stacks, repositories, profile and agent runs. Each is
// derived from the repositories this browser has opened, because the RPC surface has no
// cross-repository listing and no "who am I" call (see api/home.ts).
import { AgentRunState } from "@asc/asc-client/agents";
import { type ReactNode, useMemo } from "react";
import { useHome } from "../api/home";
import { useApp } from "../api/queries";
import { type SegmentState } from "../api/needs";
import { Badge, Empty, Page, Panel, type Tone } from "../components/bits";
import { RowsSkeleton } from "../components/skeleton";
import { useAgents } from "../lib/agents";
import { useToken } from "../lib/auth";
import { useQueries } from "../lib/query";
import { useRecent } from "../lib/recent";
import { Link, useRouteId } from "../lib/router";
import { agoShort } from "../lib/time";
import { useTitle } from "../lib/title";
import { useViewer } from "../lib/viewer";
import { OpenRepo } from "./open-repo";

const SEGMENT: Record<SegmentState, string> = {
  done: "Approved or landed",
  waiting: "Waiting for review",
  idle: "Draft",
};

function SignInNote(): ReactNode {
  return (
    <Empty>
      <Link to="/signin">Sign in</Link> to see this page.
    </Empty>
  );
}

function Stacks(): ReactNode {
  const home = useHome(true);
  return (
    <Page title="My stacks" sub="Every stack in the repositories you have opened.">
      <Panel label="Stacks">
        {home.loading && home.stacks.length === 0 ? (
          <RowsSkeleton rows={3} rowHeight={56} label="Loading stacks" />
        ) : home.stacks.length === 0 ? (
          <Empty>
            No stacks yet. Submit one with <code>asc submit -s</code>.
          </Empty>
        ) : (
          <ul className="m-0 list-none p-0">
            {home.stacks.map((s) => (
              <li key={`${s.repo}:${s.ref}`} className="border-b border-line last:border-b-0">
                <Link
                  to={s.href}
                  className="flex min-h-14 flex-wrap items-center gap-x-4 gap-y-1 px-4 py-2 no-underline"
                >
                  <div className="min-w-0 flex-1 basis-48">
                    <div className="font-medium text-fg-strong">{s.name}</div>
                    <div className="text-xs text-fg-muted">{s.repo}</div>
                  </div>
                  <ol
                    className="m-0 flex list-none gap-1 p-0"
                    aria-label={s.segments.map((x) => SEGMENT[x]).join(", ")}
                  >
                    {s.segments.map((x, i) => (
                      // A stack's segments have no identity other than their position.
                      // oxlint-disable-next-line react/no-array-index-key
                      <li key={i} aria-hidden="true">
                        <span
                          className={`block h-2 w-6 rounded-full ${
                            x === "done" ? "bg-good" : x === "waiting" ? "bg-warn" : "bg-line"
                          }`}
                        />
                      </li>
                    ))}
                  </ol>
                </Link>
              </li>
            ))}
          </ul>
        )}
      </Panel>
    </Page>
  );
}

function Repos(): ReactNode {
  const recent = useRecent();
  return (
    <Page title="Repositories" sub="The repositories this browser has opened, newest first.">
      <Panel label="Repositories">
        {recent.length === 0 ? (
          <Empty>No repositories opened yet.</Empty>
        ) : (
          <ul className="m-0 list-none p-0">
            {recent.map((r) => (
              <li key={r} className="border-b border-line last:border-b-0">
                <Link to={`/${r}`} className="flex min-h-12 items-center px-4 font-medium">
                  {r}
                </Link>
              </li>
            ))}
          </ul>
        )}
      </Panel>
      <h2 className="mb-2 mt-6 text-lg font-semibold text-fg-strong">Open a repository</h2>
      <OpenRepo />
    </Page>
  );
}

function Me(): ReactNode {
  const viewer = useViewer();
  const recent = useRecent();
  return (
    <Page title={viewer?.name ?? "Your profile"} sub="This browser's view of you.">
      <Panel label="Profile">
        <dl className="m-0">
          <div className="flex min-h-11 flex-wrap items-center gap-x-4 border-b border-line px-4 py-2">
            <dt className="w-44 text-sm text-fg-muted">Display name</dt>
            <dd className="m-0 text-sm">
              {viewer?.name ?? "Not set. Add one on the sign-in form."}
            </dd>
          </div>
          <div className="flex min-h-11 flex-wrap items-center gap-x-4 px-4 py-2">
            <dt className="w-44 text-sm text-fg-muted">Repositories opened</dt>
            <dd className="m-0 text-sm">{recent.length}</dd>
          </div>
        </dl>
      </Panel>
      <p className="mt-4 text-sm">
        Theme and agent settings are in <Link to="/settings">Settings</Link>.
      </p>
    </Page>
  );
}

const STATE: Record<number, { text: string; tone: Tone }> = {
  [AgentRunState.UNSPECIFIED]: { text: "Unknown", tone: "plain" },
  [AgentRunState.QUEUED]: { text: "Queued", tone: "plain" },
  [AgentRunState.RUNNING]: { text: "Running", tone: "warn" },
  [AgentRunState.WAITING_INPUT]: { text: "Needs input", tone: "warn" },
  [AgentRunState.SUCCEEDED]: { text: "Succeeded", tone: "good" },
  [AgentRunState.FAILED]: { text: "Failed", tone: "bad" },
  [AgentRunState.CANCELLED]: { text: "Cancelled", tone: "plain" },
  [AgentRunState.BUDGET_EXCEEDED]: { text: "Over budget", tone: "bad" },
  [AgentRunState.EXPIRED]: { text: "Expired", tone: "bad" },
};

function Agents(): ReactNode {
  const { queries } = useApp();
  const recent = useRecent();
  const repos = useMemo(() => recent.slice(0, 6), [recent]);
  const results = useQueries(
    useMemo(() => repos.map((r) => queries.agentRuns(r)), [repos, queries]),
  );
  const runs = results.flatMap((e) => e.data ?? []).toSorted((a, b) => b.created - a.created);
  const loading = results.some((e) => e.status === "pending");
  return (
    <Page
      title="Agent runs"
      sub={
        <>
          Runs in the repositories you have opened. Start one with <code>asc agent run</code>; see{" "}
          <Link to="/agents/new">how</Link>.
        </>
      }
    >
      <Panel label="Agent runs">
        {loading && runs.length === 0 ? (
          <RowsSkeleton rows={3} rowHeight={56} label="Loading agent runs" />
        ) : runs.length === 0 ? (
          <Empty>No agent runs in these repositories.</Empty>
        ) : (
          <ul className="m-0 list-none p-0">
            {runs.map((r) => (
              <li
                key={`${r.repo}:${r.id}`}
                className="flex min-h-14 flex-wrap items-center gap-x-4 gap-y-1 border-b border-line px-4 py-2 last:border-b-0"
              >
                <div className="min-w-0 flex-1 basis-60">
                  <div className="font-medium text-fg-strong">{r.title || `Run ${r.id}`}</div>
                  <div className="text-xs text-fg-muted">
                    {r.repo} · agent:{r.agent} · started by {r.createdBy || "unknown"}
                    {r.created ? ` · ${agoShort(r.created)}` : ""}
                    {r.changes > 0
                      ? ` · ${r.changes} ${r.changes === 1 ? "change" : "changes"}`
                      : ""}
                  </div>
                  {r.failure ? <div className="text-xs text-bad">{r.failure}</div> : null}
                </div>
                <Badge tone={STATE[r.state]?.tone ?? "plain"}>
                  {STATE[r.state]?.text ?? "Unknown"}
                </Badge>
              </li>
            ))}
          </ul>
        )}
      </Panel>
    </Page>
  );
}

function AgentsNew(): ReactNode {
  return (
    <Page
      title="Start an agent run"
      sub="An agent run is started from the command line, so its task, budget and key stay in your shell history and config."
    >
      <Panel>
        <div className="grid gap-3 p-4">
          <pre
            className="m-0 overflow-x-auto rounded-md border border-line bg-subtle p-3 font-mono text-[13px]"
            tabIndex={0}
            aria-label="Start an agent run"
          >{`asc agent run --title "Add retry policy" --max-cost 2.50 -w "Add a retry policy to the HTTP client"`}</pre>
          <p className="text-sm">
            The run's changes arrive as a draft stack under Changes, for you to review. Follow a run
            with <code>asc agent watch</code>, stop it with <code>asc agent cancel</code>.
          </p>
        </div>
      </Panel>
    </Page>
  );
}

export default function ListsPage(): ReactNode {
  const id = useRouteId();
  const { auth } = useApp();
  const signedIn = useToken(auth) !== undefined;
  const agents = useAgents();
  const title =
    {
      stacks: "My stacks",
      repos: "Repositories",
      me: "Your profile",
      agents: "Agent runs",
      agentsNew: "Start an agent run",
    }[id ?? ""] ?? "ASC Hub";
  useTitle(title);
  if (id === "repos") return <Repos />;
  if (!signedIn) {
    return (
      <Page title={title}>
        <SignInNote />
      </Page>
    );
  }
  if (id === "stacks") return <Stacks />;
  if (id === "me") return <Me />;
  if ((id === "agents" || id === "agentsNew") && !agents.on) {
    return (
      <Page title="Agent runs">
        <Empty>
          Agent features are off in this browser. Turn them on in{" "}
          <Link to="/settings">Settings</Link>.
        </Empty>
      </Page>
    );
  }
  return id === "agentsNew" ? <AgentsNew /> : <Agents />;
}
