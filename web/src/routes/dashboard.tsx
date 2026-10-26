// The signed-in home: what needs your attention, your recent changes and your repositories.
// asc has no "list my repositories" RPC yet, so the repositories are the ones this browser has opened
// (src/lib/recent.ts). Everything else is read through the same cached queries the other screens use,
// for at most a handful of repositories, so the page costs a few small calls and nothing blocks paint.
import { type ReactNode, useMemo, useState } from "react";
import { useApp, useQ } from "../api/queries";
import type { InboxItemView } from "../api/inbox";
import type { ChangeRow } from "../api/types";
import { Icon, type IconName } from "../components/icons";
import { Bar } from "../components/skeleton";
import { useAgents } from "../lib/agents";
import { detectLanguage } from "../lib/language";
import { openPalette } from "../lib/palette";
import { changeHref, repoHref } from "../lib/paths";
import { parseRepoName, remember, useRecent } from "../lib/recent";
import { useQueries } from "../lib/query";
import { Link, useNavigate } from "../lib/router";
import { ago } from "../lib/time";
import { useTitle } from "../lib/title";

const CARD = "rounded-xl border border-line bg-surface";
const CARD_HEAD = "flex items-center gap-2 border-b border-line px-4";
const TOP_REPOS = 4;
const INBOX_REPOS = 5;
const ROW = 64;

function splitName(name: string): { owner: string; repo: string } {
  return parseRepoName(name) ?? { owner: name, repo: "" };
}

// --- numbers ------------------------------------------------------------------------------------

function useDashboardData(recent: readonly string[]) {
  const { queries } = useApp();
  const changeKey = recent.slice(0, TOP_REPOS).join("\n");
  const inboxKey = recent.slice(0, INBOX_REPOS).join("\n");
  const changeNames = useMemo(() => (changeKey ? changeKey.split("\n") : []), [changeKey]);
  const inboxNames = useMemo(() => (inboxKey ? inboxKey.split("\n") : []), [inboxKey]);
  const changes = useQueries(
    useMemo(() => changeNames.map((n) => queries.changes(n)), [changeNames, queries]),
  );
  const inbox = useQueries(
    useMemo(() => inboxNames.map((n) => queries.inbox(n)), [inboxNames, queries]),
  );
  const rows = changes.flatMap((e, i) =>
    (e.data?.stacks ?? []).flatMap((s) =>
      s.changes.filter((c) => !c.landed).map((row) => ({ repo: changeNames[i] ?? "", row })),
    ),
  );
  const turn = inbox.flatMap((e, i) =>
    (e.data?.items ?? []).map((item) => ({ repo: inboxNames[i] ?? "", item })),
  );
  return {
    rows,
    turn,
    loadingChanges: changes.some((e) => e.status === "pending"),
    loadingInbox: inbox.some((e) => e.status === "pending"),
  };
}

type Data = ReturnType<typeof useDashboardData>;

const isFailing = (r: ChangeRow) => r.review?.checks === "failure";
const isReady = (r: ChangeRow) =>
  !!r.review && !r.review.draft && r.review.approvals > 0 && r.review.checks === "success";

// --- header and stats ---------------------------------------------------------------------------

function Stat(props: {
  to: string;
  icon: IconName;
  label: string;
  value: number | undefined;
  tone: "turn" | "bad" | "good" | "plain";
}): ReactNode {
  const tone = {
    turn: "bg-[rgb(var(--color-accent-emerald)/0.16)] text-signal",
    bad: "bg-danger-bg text-bad",
    good: "bg-note-bg text-note",
    plain: "bg-subtle-hover text-fg-muted",
  }[props.tone];
  return (
    <Link
      to={props.to}
      className={`${CARD} lp-card flex items-center gap-3 px-4 text-fg no-underline hover:bg-subtle`}
      style={{ height: 84 }}
    >
      <span className={`flex h-10 w-10 shrink-0 items-center justify-center rounded-xl ${tone}`}>
        <Icon name={props.icon} size={20} />
      </span>
      <span className="min-w-0">
        <span className="block text-2xl font-bold leading-7 tabular-nums text-fg-strong">
          {props.value === undefined ? <Bar width="2.5rem" height={24} /> : props.value}
        </span>
        <span className="block truncate text-sm text-fg-muted">{props.label}</span>
      </span>
    </Link>
  );
}

function Stats(props: { data: Data; firstRepo: string | undefined }): ReactNode {
  const { rows, turn, loadingChanges, loadingInbox } = props.data;
  const own = turn.filter((t) => t.item.section !== 4).length;
  const changesTo = props.firstRepo ? `/${props.firstRepo}/changes` : "/";
  return (
    <ul className="grid grid-cols-2 gap-3 lg:grid-cols-4" aria-label="At a glance">
      <li>
        <Stat
          to="/inbox"
          icon="inbox"
          label="Your turn"
          value={loadingInbox ? undefined : own}
          tone="turn"
        />
      </li>
      <li>
        <Stat
          to={changesTo}
          icon="layers"
          label="Open changes"
          value={loadingChanges ? undefined : rows.length}
          tone="plain"
        />
      </li>
      <li>
        <Stat
          to={changesTo}
          icon="alert"
          label="Checks failing"
          value={loadingChanges ? undefined : rows.filter((r) => isFailing(r.row)).length}
          tone="bad"
        />
      </li>
      <li>
        <Stat
          to={changesTo}
          icon="merge"
          label="Ready to land"
          value={loadingChanges ? undefined : rows.filter((r) => isReady(r.row)).length}
          tone="good"
        />
      </li>
    </ul>
  );
}

// --- your turn ----------------------------------------------------------------------------------

const SECTION: Record<number, { label: string; tone: string; turn: boolean }> = {
  1: {
    label: "Review requested",
    tone: "bg-[rgb(var(--color-accent-emerald)/0.16)] text-signal",
    turn: true,
  },
  2: {
    label: "Feedback to address",
    tone: "bg-[rgb(var(--color-accent-emerald)/0.16)] text-signal",
    turn: true,
  },
  3: { label: "Checks failed", tone: "bg-danger-bg text-bad", turn: true },
  4: { label: "Waiting on others", tone: "bg-subtle-hover text-fg-muted", turn: false },
};

function TurnRow(props: { repo: string; item: InboxItemView }): ReactNode {
  const { owner, repo } = splitName(props.repo);
  const s = SECTION[props.item.section] ?? SECTION[4];
  const initial = (props.item.author || "?").slice(0, 1).toUpperCase();
  return (
    <li>
      <Link
        to={changeHref(owner, repo, props.item.change)}
        className="flex items-center gap-3 px-4 text-fg no-underline hover:bg-subtle"
        style={{ minHeight: 60 }}
      >
        <span
          aria-hidden="true"
          className="flex h-8 w-8 shrink-0 items-center justify-center rounded-full bg-brand text-sm font-semibold text-on-brand"
        >
          {initial}
        </span>
        <span className="min-w-0 flex-1">
          <span className="flex items-baseline gap-2 text-sm font-semibold text-fg-strong">
            <span className="min-w-0 truncate">{props.repo}</span>
            <span className="font-mono text-xs font-normal text-fg-muted">
              {props.item.change.slice(0, 6)}
            </span>
          </span>
          <span className="mt-1 flex items-center gap-2 text-xs text-fg-muted">
            <span className={`whitespace-nowrap rounded-full px-2 py-0.5 font-semibold ${s?.tone}`}>
              {s?.label}
            </span>
            <span className="truncate">
              {props.item.author ? `${props.item.author} · ` : ""}
              {ago(props.item.updated)}
            </span>
          </span>
        </span>
      </Link>
    </li>
  );
}

function YourTurn(props: { data: Data }): ReactNode {
  const items = props.data.turn
    .toSorted(
      (a, b) =>
        Number(SECTION[b.item.section]?.turn) - Number(SECTION[a.item.section]?.turn) ||
        b.item.updated - a.item.updated,
    )
    .slice(0, 6);
  return (
    <section aria-labelledby="turn-h" className={`${CARD} overflow-hidden`}>
      <div className={CARD_HEAD} style={{ height: 48 }}>
        <h2 id="turn-h" className="flex-1 text-sm font-semibold text-fg-strong">
          Your turn
        </h2>
        <Link
          to="/inbox"
          className="inline-flex min-h-11 items-center text-sm no-underline hover:underline sm:min-h-0"
        >
          Inbox
        </Link>
      </div>
      {props.data.loadingInbox && items.length === 0 ? (
        <div
          role="status"
          aria-busy="true"
          aria-label="Loading your turn"
          className="divide-y divide-line"
        >
          {[0, 1, 2, 3, 4].map((i) => (
            <div key={i} className="flex items-center px-4" style={{ height: 60 }}>
              <Bar width={`${90 - i * 9}%`} height={18} />
            </div>
          ))}
        </div>
      ) : items.length === 0 ? (
        <div className="px-4 py-8 text-center">
          <span className="mx-auto mb-2 flex h-10 w-10 items-center justify-center rounded-full bg-[rgb(var(--color-accent-emerald)/0.16)] text-signal">
            <Icon name="check" size={20} />
          </span>
          <p className="text-sm font-semibold text-fg-strong">Nothing waiting on you</p>
          <p className="mt-1 text-sm text-fg-muted">
            Reviews, feedback and failed checks show up here.
          </p>
        </div>
      ) : (
        <ul className="divide-y divide-line">
          {items.map((t) => (
            <TurnRow
              key={`${t.repo}:${t.item.change}:${t.item.section}`}
              repo={t.repo}
              item={t.item}
            />
          ))}
        </ul>
      )}
    </section>
  );
}

// --- recent changes -----------------------------------------------------------------------------

function ChecksIcon(props: { checks: string }): ReactNode {
  if (props.checks === "success")
    return (
      <span className="inline-flex items-center gap-1 text-good">
        <Icon name="check" size={13} />
        Checks passed
      </span>
    );
  if (props.checks === "failure")
    return (
      <span className="inline-flex items-center gap-1 text-bad">
        <Icon name="x" size={13} />
        Checks failed
      </span>
    );
  if (props.checks === "pending")
    return (
      <span className="inline-flex items-center gap-1 text-warn">
        <Icon name="clock" size={13} />
        Checks running
      </span>
    );
  return null;
}

function ChangeLine(props: { repo: string; row: ChangeRow }): ReactNode {
  const { owner, repo } = splitName(props.repo);
  const { row } = props;
  return (
    <li>
      <Link
        to={changeHref(owner, repo, row.id)}
        className="flex items-center gap-3 px-4 text-fg no-underline hover:bg-subtle"
        style={{ height: ROW }}
      >
        <span className="flex h-8 w-8 shrink-0 items-center justify-center rounded-lg bg-subtle-hover text-fg-muted">
          <Icon name="layers" size={16} />
        </span>
        <span className="min-w-0 flex-1">
          <span className="block truncate text-sm font-semibold text-fg-strong">{row.title}</span>
          <span className="flex items-center gap-x-3 truncate text-xs text-fg-muted">
            <span className="truncate">{props.repo}</span>
            <span className="whitespace-nowrap">
              {row.position + 1} of {row.stackSize}
            </span>
            {row.review?.draft ? <span className="text-warn">Draft</span> : null}
            {row.needsRestack ? <span className="text-warn">Needs restack</span> : null}
          </span>
        </span>
        <span className="hidden items-center gap-4 whitespace-nowrap text-xs sm:flex">
          {row.review && row.review.approvals > 0 ? (
            <span className="text-fg-muted">{row.review.approvals} approved</span>
          ) : null}
          {row.review ? <ChecksIcon checks={row.review.checks} /> : null}
        </span>
      </Link>
    </li>
  );
}

function RecentChanges(props: { data: Data; empty: boolean }): ReactNode {
  const { rows, loadingChanges } = props.data;
  // Failing first (they need you), then ready, then the rest, each in stack order top first.
  const ranked = rows
    .toSorted(
      (a, b) =>
        Number(isFailing(b.row)) - Number(isFailing(a.row)) ||
        Number(isReady(b.row)) - Number(isReady(a.row)) ||
        b.row.position - a.row.position,
    )
    .slice(0, 6);
  return (
    <section aria-labelledby="changes-h" className={`${CARD} overflow-hidden`}>
      <div className={CARD_HEAD} style={{ height: 48 }}>
        <h2 id="changes-h" className="flex-1 text-sm font-semibold text-fg-strong">
          Recent changes
        </h2>
      </div>
      {loadingChanges && ranked.length === 0 && !props.empty ? (
        <div role="status" aria-busy="true" aria-label="Loading changes">
          {[0, 1, 2, 3, 4, 5].map((i) => (
            <div
              key={i}
              className="flex items-center gap-3 border-b border-line px-4"
              style={{ height: ROW }}
            >
              <Bar width="2rem" height={32} />
              <div className="flex-1 space-y-2">
                <Bar width={`${60 - i * 8}%`} height={14} />
                <Bar width="30%" height={12} />
              </div>
            </div>
          ))}
        </div>
      ) : ranked.length === 0 ? (
        <div className="px-4 py-10 text-center">
          <p className="text-sm font-semibold text-fg-strong">No open changes</p>
          <p className="mt-1 text-sm text-fg-muted">
            Start one with <code className="font-mono">asc create -m "message"</code>, then{" "}
            <code className="font-mono">asc submit</code>.
          </p>
        </div>
      ) : (
        <ul className="divide-y divide-line">
          {ranked.map((r) => (
            <ChangeLine key={`${r.repo}:${r.row.id}`} repo={r.repo} row={r.row} />
          ))}
        </ul>
      )}
    </section>
  );
}

// --- repositories -------------------------------------------------------------------------------

function RepoRow(props: { name: string }): ReactNode {
  const { queries } = useApp();
  const { owner, repo } = splitName(props.name);
  const refs = useQ(queries.refs(props.name));
  const branch = refs.data?.defaultBranch ?? "";
  const commit = useQ(queries.lastCommit(props.name, branch, ""));
  const tree = useQ(queries.tree(props.name, branch, ""));
  const lang = tree.data?.done ? detectLanguage(tree.data.rows.map((r) => r.name)) : undefined;
  const failed = refs.status === "error";
  return (
    <li className="flex items-center gap-3 px-4" style={{ height: ROW }}>
      <span className="flex h-8 w-8 shrink-0 items-center justify-center rounded-lg bg-subtle-hover text-fg-muted">
        <Icon name="repo" size={16} />
      </span>
      <div className="min-w-0 flex-1">
        <Link
          to={repoHref(owner, repo)}
          className="block truncate text-sm font-semibold no-underline hover:underline"
        >
          <span className="font-normal text-fg-muted">{owner} / </span>
          {repo}
        </Link>
        <div className="truncate text-xs text-fg-muted" style={{ height: 18 }}>
          {commit.data ? (
            commit.data.subject
          ) : failed ? (
            "Not available"
          ) : (
            <Bar width="55%" height={12} />
          )}
        </div>
      </div>
      <div
        className="hidden w-32 items-center gap-1.5 text-xs text-fg-muted md:flex"
        style={{ height: 18 }}
      >
        {lang ? (
          <>
            <span
              aria-hidden="true"
              className="h-3 w-3 rounded-full"
              style={{ background: lang.color }}
            />
            {lang.name}
          </>
        ) : null}
      </div>
      <div className="hidden w-24 items-center gap-1 text-xs text-fg-muted sm:flex">
        {branch ? (
          <>
            <Icon name="branch" size={13} />
            <span className="truncate">{branch}</span>
          </>
        ) : null}
      </div>
      <div className="w-20 text-right text-xs text-fg-muted">
        {commit.data ? ago(commit.data.time) : null}
      </div>
    </li>
  );
}

function OpenForm(): ReactNode {
  const navigate = useNavigate();
  const [value, setValue] = useState("");
  const [invalid, setInvalid] = useState(false);
  return (
    <form
      onSubmit={(e) => {
        e.preventDefault();
        const p = parseRepoName(value);
        setInvalid(!p);
        if (!p) return;
        remember(`${p.owner}/${p.repo}`);
        navigate(repoHref(p.owner, p.repo));
      }}
    >
      <label htmlFor="open-repo" className="mb-1 block text-sm font-medium text-fg-strong">
        Repository name
      </label>
      <div className="flex gap-2">
        <input
          id="open-repo"
          value={value}
          onChange={(e) => setValue(e.target.value)}
          placeholder="owner/repo"
          autoComplete="off"
          spellCheck={false}
          aria-invalid={invalid}
          aria-describedby="open-repo-err"
          className="h-11 min-w-0 flex-1 rounded-lg border border-control bg-page px-3 text-sm sm:h-9"
        />
        <button
          type="submit"
          className="h-11 rounded-lg bg-brand px-4 text-sm font-semibold text-on-brand sm:h-9"
        >
          Open
        </button>
      </div>
      <p
        id="open-repo-err"
        className={`mt-1 min-h-5 text-sm ${invalid ? "text-bad" : "text-transparent"}`}
      >
        {invalid ? "Use the form owner/repo." : "."}
      </p>
    </form>
  );
}

function Repositories(props: { names: readonly string[] }): ReactNode {
  const [filter, setFilter] = useState("");
  const shown = props.names.filter((n) => n.toLowerCase().includes(filter.trim().toLowerCase()));
  return (
    <section aria-labelledby="repos-h" className={`${CARD} overflow-hidden`}>
      <div className={CARD_HEAD} style={{ height: 56 }}>
        <h2 id="repos-h" className="text-sm font-semibold text-fg-strong">
          Repositories
        </h2>
        <span className="flex-1" />
        <label htmlFor="repo-filter" className="sr-only">
          Find a repository
        </label>
        <input
          id="repo-filter"
          type="search"
          value={filter}
          onChange={(e) => setFilter(e.target.value)}
          placeholder="Find a repository…"
          autoComplete="off"
          className="h-11 w-40 min-w-0 rounded-lg border border-control bg-page px-3 text-sm sm:h-8 sm:w-56"
        />
      </div>
      {shown.length === 0 ? (
        <p className="px-4 py-10 text-center text-sm text-fg-muted">
          {props.names.length === 0
            ? "No repositories yet."
            : `No repository matches “${filter.trim()}”.`}
        </p>
      ) : (
        <ul className="divide-y divide-line" aria-label="Repositories">
          {shown.map((n) => (
            <RepoRow key={n} name={n} />
          ))}
        </ul>
      )}
    </section>
  );
}

// --- side column --------------------------------------------------------------------------------

function Kbd(props: { children: ReactNode }): ReactNode {
  return (
    <kbd className="rounded border border-control bg-subtle px-1.5 font-mono text-xs text-fg">
      {props.children}
    </kbd>
  );
}

const TIPS: readonly { keys: readonly string[]; label: string }[] = [
  { keys: ["/"], label: "Search" },
  { keys: ["Ctrl", "K"], label: "Command palette" },
  { keys: ["g", "i"], label: "Inbox" },
  { keys: ["g", "p"], label: "Changes in a repository" },
  { keys: ["t"], label: "Go to file" },
  { keys: ["?"], label: "All shortcuts" },
];

function Side(): ReactNode {
  return (
    <div className="space-y-4">
      <section aria-labelledby="open-h" className={`${CARD} p-4`}>
        <h2 id="open-h" className="mb-3 text-sm font-semibold text-fg-strong">
          Open a repository
        </h2>
        <OpenForm />
      </section>
      <section aria-labelledby="keys-h" className={`${CARD} hidden p-4 lg:block`}>
        <h2 id="keys-h" className="mb-3 text-sm font-semibold text-fg-strong">
          Keyboard
        </h2>
        <ul className="space-y-2 text-sm">
          {TIPS.map((t) => (
            <li key={t.label} className="flex items-center justify-between gap-3">
              <span className="text-fg-muted">{t.label}</span>
              <span className="flex items-center gap-1 whitespace-nowrap">
                {t.keys.map((k, i) => (
                  <span key={k} className="flex items-center gap-1">
                    {i > 0 && t.keys[0] !== "Ctrl" ? (
                      <span className="text-xs text-fg-muted">then</span>
                    ) : null}
                    <Kbd>{k}</Kbd>
                  </span>
                ))}
              </span>
            </li>
          ))}
        </ul>
      </section>
    </div>
  );
}

function Onboarding(): ReactNode {
  return (
    <section aria-labelledby="start-h" className={`${CARD} px-6 py-12 text-center`}>
      <span className="mx-auto mb-4 flex h-12 w-12 items-center justify-center rounded-2xl bg-[rgb(var(--color-primary)/0.12)] text-link">
        <Icon name="repo" size={24} />
      </span>
      <h2 id="start-h" className="text-lg font-semibold text-fg-strong">
        Open your first repository
      </h2>
      <p className="mx-auto mt-1 max-w-md text-sm text-fg-muted">
        Type a name such as <code className="font-mono">acme/app</code> in the box, or press{" "}
        <Kbd>Ctrl</Kbd> <Kbd>K</Kbd>. The repositories you open are listed here, in this browser
        only.
      </p>
      <button
        type="button"
        onClick={() => openPalette()}
        className="mt-5 inline-flex h-11 items-center gap-2 rounded-lg bg-brand px-4 text-sm font-semibold text-on-brand sm:h-9"
      >
        <Icon name="search" size={14} />
        Find a repository
      </button>
    </section>
  );
}

/** Shown only with "Show agent features" on. Nothing here is fetched. */
function AgentsNote(): ReactNode {
  return (
    <section aria-labelledby="agents-h" className={`${CARD} p-4`}>
      <h2
        id="agents-h"
        className="mb-1 flex items-center gap-2 text-sm font-semibold text-fg-strong"
      >
        <Icon name="bot" size={16} />
        Agent work
      </h2>
      <p className="text-sm text-fg-muted">
        Agent features are on. Changes made by agents are listed with the others, with the agent as
        the author. Turn this off in{" "}
        <Link to="/settings" className="underline">
          Settings
        </Link>
        .
      </p>
    </section>
  );
}

function Greeting(props: { data: Data }): ReactNode {
  const own = props.data.turn.filter((t) => t.item.section !== 4).length;
  const text = props.data.loadingInbox
    ? " "
    : own === 0
      ? "Nothing is waiting on you."
      : `${own} ${own === 1 ? "thing needs" : "things need"} you.`;
  return (
    <div className="min-w-0">
      <h1 className="text-2xl font-extrabold tracking-tight text-fg-strong">Dashboard</h1>
      <p className="mt-0.5 min-h-5 text-sm text-fg-muted">{text}</p>
    </div>
  );
}

export default function Dashboard(): ReactNode {
  useTitle("Dashboard");
  const recent = useRecent();
  const data = useDashboardData(recent);
  const empty = recent.length === 0;
  const agents = useAgents();
  return (
    // min-h keeps the footer below the fold while data arrives, so it does not jump out of view.
    <div className="mx-auto min-h-dvh w-full max-w-7xl px-4 py-6 sm:px-6">
      <div className="mb-5 flex flex-wrap items-center gap-3">
        <Greeting data={data} />
        <span className="flex-1" />
        <button
          type="button"
          onClick={() => openPalette()}
          className="inline-flex h-11 items-center gap-2 rounded-lg border border-control bg-surface px-3 text-sm text-fg hover:bg-subtle-hover sm:h-9"
        >
          <Icon name="search" size={14} />
          Jump to…
          <span className="hidden sm:inline">
            <Kbd>Ctrl K</Kbd>
          </span>
        </button>
      </div>
      <div className="grid grid-cols-[minmax(0,1fr)] gap-5 lg:grid-cols-[minmax(0,1fr)_20rem] lg:items-start">
        <div className="contents lg:block lg:min-w-0 lg:space-y-5">
          <div className="order-1 lg:order-none">
            <Stats data={data} firstRepo={recent[0]} />
          </div>
          <div className="order-3 lg:order-none">
            <RecentChanges data={data} empty={empty} />
          </div>
          <div className="order-4 lg:order-none">
            {empty ? <Onboarding /> : <Repositories names={recent} />}
          </div>
        </div>
        <div className="contents lg:block lg:min-w-0 lg:space-y-4">
          <div className="order-2 lg:order-none">
            <YourTurn data={data} />
          </div>
          {agents.on ? (
            <div className="order-5 lg:order-none">
              <AgentsNote />
            </div>
          ) : null}
          <div className="order-6 lg:order-none">
            <Side />
          </div>
        </div>
      </div>
    </div>
  );
}
