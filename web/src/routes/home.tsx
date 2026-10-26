// Dashboard. asc has no "list my repositories" RPC yet, so v1 opens a repository by name and
// remembers the last few in this browser (a per-viewer convenience, never required). Each remembered
// repository shows its default branch and last commit, read through the same cached queries the
// repo page uses, so opening it afterwards is instant.
import { type ReactNode, useState } from "react";
import { useApp, useQ } from "../api/queries";
import { Icon } from "../components/icons";
import { Bar } from "../components/skeleton";
import { useToken } from "../lib/auth";
import { Link, useNavigate } from "../lib/router";
import { repoHref } from "../lib/paths";
import { ago } from "../lib/time";
import { useTitle } from "../lib/title";

const RECENT_KEY = "asc-recent";
const MAX_RECENT = 8;

export function parseRepoName(input: string): { owner: string; repo: string } | undefined {
  const m = /^\/?([\w.-]+)\/([\w.-]+?)(?:\.git)?\/?$/.exec(input.trim());
  return m?.[1] && m[2] ? { owner: m[1], repo: m[2] } : undefined;
}

function readRecent(): string[] {
  try {
    const raw: unknown = JSON.parse(localStorage.getItem(RECENT_KEY) ?? "[]");
    return Array.isArray(raw)
      ? raw
          .filter((x): x is string => typeof x === "string" && !!parseRepoName(x))
          .slice(0, MAX_RECENT)
      : [];
  } catch {
    return [];
  }
}

function remember(name: string): void {
  try {
    const next = [name, ...readRecent().filter((r) => r !== name)].slice(0, MAX_RECENT);
    localStorage.setItem(RECENT_KEY, JSON.stringify(next));
  } catch {
    // Storage may be blocked; recents are optional.
  }
}

const ROW_HEIGHT = 72;

/** The second line of a recent repository: last commit on the default branch. */
function LastCommit(props: { name: string; rev: string }): ReactNode {
  const { queries } = useApp();
  const commit = useQ(queries.lastCommit(props.name, props.rev, ""));
  const c = commit.data;
  if (!c) {
    return commit.status === "error" ? (
      <span className="text-fg-muted">Not available</span>
    ) : (
      <Bar width="55%" height={12} />
    );
  }
  return (
    <>
      <span className="min-w-0 truncate">{c.subject}</span>
      <span className="whitespace-nowrap">· {ago(c.time)}</span>
    </>
  );
}

function RepoMeta(props: { name: string }): ReactNode {
  const { queries } = useApp();
  const refs = useQ(queries.refs(props.name));
  return (
    <div className="mt-1 flex items-center gap-2 text-sm text-fg-muted" style={{ height: 20 }}>
      {refs.data ? (
        <>
          <span className="inline-flex items-center gap-1 whitespace-nowrap">
            <Icon name="branch" size={14} />
            {refs.data.defaultBranch}
          </span>
          <LastCommit name={props.name} rev={refs.data.defaultBranch} />
        </>
      ) : refs.status === "error" ? (
        <span>Not available</span>
      ) : (
        <Bar width="40%" height={12} />
      )}
    </div>
  );
}

function RecentList(props: { names: readonly string[] }): ReactNode {
  return (
    <ul className="divide-y divide-line">
      {props.names.map((name) => {
        const p = parseRepoName(name);
        return p ? (
          <li key={name} className="px-4 py-3" style={{ minHeight: ROW_HEIGHT }}>
            <div className="flex items-center gap-2">
              <span className="text-fg-muted">
                <Icon name="repo" />
              </span>
              <Link
                to={repoHref(p.owner, p.repo)}
                className="min-w-0 truncate font-semibold no-underline hover:underline"
              >
                {p.owner} / {p.repo}
              </Link>
            </div>
            <RepoMeta name={name} />
          </li>
        ) : null;
      })}
    </ul>
  );
}

function EmptyRecent(): ReactNode {
  return (
    <div className="px-6 py-12 text-center">
      <div className="mx-auto mb-3 flex h-10 w-10 items-center justify-center rounded-full bg-subtle text-fg-muted">
        <Icon name="repo" size={20} />
      </div>
      <p className="mb-1 font-semibold text-fg-strong">No repositories yet</p>
      <p className="text-sm text-fg-muted">
        Open one by name, for example <code className="font-mono">acme/app</code>. The ones you open
        are listed here, in this browser only.
      </p>
    </div>
  );
}

const SHORTCUTS: readonly { keys: readonly string[]; label: string }[] = [
  { keys: ["?"], label: "All shortcuts" },
  { keys: ["/"], label: "Search" },
  { keys: ["g", "h"], label: "Dashboard" },
  { keys: ["g", "i"], label: "Inbox" },
  { keys: ["g", "s"], label: "Settings" },
];

function Kbd(props: { children: ReactNode }): ReactNode {
  return (
    <kbd className="rounded border border-control bg-subtle px-1.5 font-mono text-xs">
      {props.children}
    </kbd>
  );
}

export default function HomePage(): ReactNode {
  useTitle("Dashboard");
  const { auth } = useApp();
  const token = useToken(auth);
  const navigate = useNavigate();
  const [value, setValue] = useState("");
  const [invalid, setInvalid] = useState(false);
  const [recent] = useState(readRecent);
  return (
    <div className="mx-auto w-full max-w-6xl p-4">
      <h1 className="mb-4 text-2xl font-semibold text-fg-strong">Dashboard</h1>
      <div className="grid gap-6 lg:grid-cols-[minmax(0,1fr)_20rem]">
        <section
          aria-labelledby="recent-h"
          className="order-2 self-start overflow-hidden rounded-lg border border-line bg-surface lg:order-1"
        >
          <h2
            id="recent-h"
            className="border-b border-line bg-subtle px-4 text-base font-semibold text-fg-strong"
            style={{ lineHeight: "44px" }}
          >
            Recent repositories
          </h2>
          {recent.length === 0 ? <EmptyRecent /> : <RecentList names={recent} />}
        </section>
        <div className="order-1 space-y-4 lg:order-2">
          <section
            aria-labelledby="open-h"
            className="rounded-lg border border-line bg-surface p-4"
          >
            <h2 id="open-h" className="mb-3 text-base font-semibold text-fg-strong">
              Open a repository
            </h2>
            <form
              onSubmit={(e) => {
                e.preventDefault();
                const parsed = parseRepoName(value);
                setInvalid(!parsed);
                if (!parsed) return;
                remember(`${parsed.owner}/${parsed.repo}`);
                navigate(repoHref(parsed.owner, parsed.repo));
              }}
            >
              <label htmlFor="open-repo" className="mb-1 block text-sm font-medium">
                Name
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
                  aria-describedby={invalid ? "open-repo-err" : undefined}
                  className="h-10 min-w-0 flex-1 rounded-md border border-control bg-page px-3"
                />
                <button
                  type="submit"
                  className="h-10 rounded-md bg-brand px-4 font-medium text-on-brand"
                >
                  Open
                </button>
              </div>
              <div style={{ minHeight: 20 }} className="mt-1">
                {invalid ? (
                  <p id="open-repo-err" className="text-sm text-bad">
                    Use the form owner/repo.
                  </p>
                ) : null}
              </div>
            </form>
          </section>
          {token ? null : (
            <section className="rounded-lg border border-line bg-surface p-4">
              <h2 className="mb-1 text-base font-semibold text-fg-strong">Private repositories</h2>
              <p className="mb-3 text-sm text-fg-muted">
                Sign in with a token to open repositories that are not public.
              </p>
              <Link
                to="/signin"
                className="inline-flex h-9 items-center rounded-md border border-control px-3 text-sm text-fg no-underline hover:bg-subtle-hover"
              >
                Sign in
              </Link>
            </section>
          )}
          <section
            aria-labelledby="keys-h"
            className="hidden rounded-lg border border-line bg-surface p-4 lg:block"
          >
            <h2 id="keys-h" className="mb-2 text-base font-semibold text-fg-strong">
              Keyboard
            </h2>
            <ul className="space-y-1.5 text-sm">
              {SHORTCUTS.map((s) => (
                <li key={s.label} className="flex items-center justify-between gap-3">
                  <span className="text-fg-muted">{s.label}</span>
                  <span className="whitespace-nowrap">
                    {s.keys.map((k, i) => (
                      <span key={k}>
                        {i > 0 ? <span className="px-1 text-fg-muted">then</span> : null}
                        <Kbd>{k}</Kbd>
                      </span>
                    ))}
                  </span>
                </li>
              ))}
            </ul>
          </section>
        </div>
      </div>
    </div>
  );
}
