// Dashboard. asc has no "list my repositories" RPC yet, so v1 opens a repository by name and
// remembers the last few in this browser (a per-viewer convenience, never required).
import { type ReactNode, useState } from "react";
import { Link, useNavigate } from "../lib/router";
import { repoHref } from "../lib/paths";
import { useTitle } from "../lib/title";

const RECENT_KEY = "asc-recent";

export function parseRepoName(input: string): { owner: string; repo: string } | undefined {
  const m = /^\/?([\w.-]+)\/([\w.-]+?)(?:\.git)?\/?$/.exec(input.trim());
  return m?.[1] && m[2] ? { owner: m[1], repo: m[2] } : undefined;
}

function readRecent(): string[] {
  try {
    const raw: unknown = JSON.parse(localStorage.getItem(RECENT_KEY) ?? "[]");
    return Array.isArray(raw)
      ? raw.filter((x): x is string => typeof x === "string" && !!parseRepoName(x)).slice(0, 8)
      : [];
  } catch {
    return [];
  }
}

function remember(name: string): void {
  try {
    const next = [name, ...readRecent().filter((r) => r !== name)].slice(0, 8);
    localStorage.setItem(RECENT_KEY, JSON.stringify(next));
  } catch {
    // Storage may be blocked; recents are optional.
  }
}

export default function HomePage(): ReactNode {
  useTitle("Dashboard");
  const navigate = useNavigate();
  const [value, setValue] = useState("");
  const [invalid, setInvalid] = useState(false);
  const [recent] = useState(readRecent);
  return (
    <div className="mx-auto w-full max-w-3xl p-4">
      <h1 className="mb-4 text-2xl font-semibold text-fg-strong">Dashboard</h1>
      <form
        className="mb-8 flex flex-wrap items-end gap-3"
        onSubmit={(e) => {
          e.preventDefault();
          const parsed = parseRepoName(value);
          setInvalid(!parsed);
          if (!parsed) return;
          remember(`${parsed.owner}/${parsed.repo}`);
          navigate(repoHref(parsed.owner, parsed.repo));
        }}
      >
        <div className="flex-1">
          <label htmlFor="open-repo" className="mb-1 block text-sm font-medium">
            Open a repository
          </label>
          <input
            id="open-repo"
            value={value}
            onChange={(e) => setValue(e.target.value)}
            placeholder="owner/repo"
            aria-invalid={invalid}
            aria-describedby={invalid ? "open-repo-err" : undefined}
            className="h-10 w-full rounded-md border border-control bg-surface px-3"
          />
          {invalid ? (
            <p id="open-repo-err" className="mt-1 text-sm text-bad">
              Use the form owner/repo.
            </p>
          ) : null}
        </div>
        <button type="submit" className="h-10 rounded-md bg-brand px-4 font-medium text-on-brand">
          Open
        </button>
      </form>
      <section aria-labelledby="recent-h" style={{ minHeight: 120 }}>
        <h2 id="recent-h" className="mb-2 text-lg font-semibold text-fg-strong">
          Recent
        </h2>
        {recent.length === 0 ? (
          <p className="text-fg-muted">Repositories you open appear here.</p>
        ) : (
          <ul className="divide-y divide-line rounded-lg border border-line bg-surface">
            {recent.map((name) => {
              const p = parseRepoName(name);
              return p ? (
                <li key={name}>
                  <Link
                    to={repoHref(p.owner, p.repo)}
                    className="block px-4 py-3 no-underline hover:bg-subtle"
                  >
                    {name}
                  </Link>
                </li>
              ) : null;
            })}
          </ul>
        )}
      </section>
    </div>
  );
}
