// Search. Inside a repository it searches the code at HEAD with the index-backed CodeIntel search
// (or finds files by name); at /search it does the same in each repository this browser has opened.
import { type FormEvent, type ReactNode, useId, useState } from "react";
import type { SearchKind } from "../api/browse";
import { useApp, useQ } from "../api/queries";
import { Empty, Page, Panel } from "../components/bits";
import { ErrorState } from "../components/error-state";
import { RowsSkeleton } from "../components/skeleton";
import { blobHref } from "../lib/paths";
import { Link, useNavigate, useParams, useRouteId, useSearchParams } from "../lib/router";
import { useRecent } from "../lib/recent";
import { useTitle } from "../lib/title";

const MODES: readonly { value: SearchKind | "files"; label: string }[] = [
  { value: "literal", label: "Text" },
  { value: "word", label: "Whole word" },
  { value: "regex", label: "Regular expression" },
  { value: "files", label: "File names" },
];

function isMode(v: string): v is SearchKind | "files" {
  return MODES.some((m) => m.value === v);
}

function FileResults(props: { repo: string; q: string }): ReactNode {
  const { queries } = useApp();
  const [owner = "", name = ""] = props.repo.split("/");
  const files = useQ(queries.findFiles(props.repo, "HEAD", props.q));
  if (files.status === "error") return <ErrorState error={files.error} />;
  if (!files.data) return <RowsSkeleton rows={3} rowHeight={40} label="Searching" />;
  if (files.data.paths.length === 0) return <Empty>No file name contains “{props.q}”.</Empty>;
  return (
    <ul className="m-0 list-none p-0">
      {files.data.paths.map((p) => (
        <li key={p} className="border-b border-line last:border-b-0">
          <Link
            to={blobHref(owner, name, "HEAD", p)}
            className="flex min-h-11 items-center break-all px-4 font-mono text-sm"
          >
            {p}
          </Link>
        </li>
      ))}
      {files.data.truncated ? (
        <li role="status" className="bg-subtle p-3 text-sm">
          Showing the first 200 files. Narrow the name to see others.
        </li>
      ) : null}
    </ul>
  );
}

function CodeResults(props: { repo: string; q: string; kind: SearchKind }): ReactNode {
  const { queries } = useApp();
  const [owner = "", name = ""] = props.repo.split("/");
  const code = useQ(queries.search(props.repo, "HEAD", props.q, props.kind));
  if (code.status === "error") return <ErrorState error={code.error} />;
  if (!code.data) return <RowsSkeleton rows={3} rowHeight={56} label="Searching" />;
  if (code.data.files.length === 0) return <Empty>No match for “{props.q}”.</Empty>;
  return (
    <ul className="m-0 list-none p-0">
      {code.data.files.map((f) => (
        <li key={f.path} className="border-b border-line px-4 py-3 last:border-b-0">
          <Link to={blobHref(owner, name, "HEAD", f.path)} className="break-all font-mono text-sm">
            {f.path}
          </Link>
          <ul className="m-0 mt-1 list-none p-0 font-mono text-[13px]">
            {f.matches.map((m) => (
              <li key={m.line} className="flex gap-3 whitespace-pre-wrap break-all">
                <Link
                  to={`${blobHref(owner, name, "HEAD", f.path)}#L${m.line}`}
                  aria-label={`Line ${m.line}`}
                  className="min-w-10 text-right text-fg-muted no-underline"
                >
                  {m.line}
                </Link>
                <span>{m.text}</span>
              </li>
            ))}
          </ul>
        </li>
      ))}
      {code.data.truncated ? (
        <li role="status" className="bg-subtle p-3 text-sm">
          The results are cut off. Narrow the pattern to see the rest.
        </li>
      ) : null}
    </ul>
  );
}

function Results(props: { repo: string; q: string; mode: SearchKind | "files" }): ReactNode {
  return props.mode === "files" ? (
    <FileResults repo={props.repo} q={props.q} />
  ) : (
    <CodeResults repo={props.repo} q={props.q} kind={props.mode} />
  );
}

export default function SearchPage(): ReactNode {
  const { owner, repo } = useParams();
  const scoped = useRouteId() === "repoSearch" && owner !== undefined && repo !== undefined;
  const params = useSearchParams();
  const q = params.get("q") ?? "";
  const rawMode = params.get("mode") ?? "literal";
  const mode = isMode(rawMode) ? rawMode : "literal";
  const navigate = useNavigate();
  const recent = useRecent();
  const [draft, setDraft] = useState(q);
  const id = useId();
  const repos = scoped ? [`${owner}/${repo}`] : recent.slice(0, 6);
  useTitle(scoped ? `Search · ${owner}/${repo}` : "Search");

  const go = (next: { q: string; mode: string }) => {
    const base = scoped ? `/${owner}/${repo}/search` : "/search";
    const qs = new URLSearchParams();
    if (next.q) qs.set("q", next.q);
    if (next.mode !== "literal") qs.set("mode", next.mode);
    const s = qs.toString();
    navigate(s ? `${base}?${s}` : base, { replace: true });
  };
  const submit = (e: FormEvent) => {
    e.preventDefault();
    go({ q: draft.trim(), mode });
  };

  return (
    <Page
      wide
      title="Search"
      sub={
        scoped ? `Code in ${owner}/${repo} at HEAD.` : "Code in the repositories you have opened."
      }
    >
      <form
        onSubmit={submit}
        aria-label="Search the code"
        className="mb-4 flex flex-wrap items-end gap-3"
      >
        <div className="min-w-0 flex-1 basis-64">
          <label htmlFor={id} className="mb-1 block text-sm font-medium text-fg-strong">
            Search for
          </label>
          <input
            id={id}
            type="text"
            value={draft}
            onChange={(e) => setDraft(e.target.value)}
            autoComplete="off"
            spellCheck={false}
            className="min-h-11 w-full rounded-md border border-line bg-surface px-3"
          />
        </div>
        <div>
          <label htmlFor={`${id}-mode`} className="mb-1 block text-sm font-medium text-fg-strong">
            Match
          </label>
          <select
            id={`${id}-mode`}
            value={mode}
            onChange={(e) => go({ q: draft.trim(), mode: e.target.value })}
            className="min-h-11 rounded-md border border-line bg-surface px-3"
          >
            {MODES.map((m) => (
              <option key={m.value} value={m.value}>
                {m.label}
              </option>
            ))}
          </select>
        </div>
        <button
          type="submit"
          className="min-h-11 cursor-pointer rounded-md border-0 bg-brand px-5 font-semibold text-on-brand"
        >
          Search
        </button>
      </form>
      {q === "" ? (
        <Panel>
          <Empty>
            {repos.length === 0
              ? "Open a repository first; search covers the repositories you have opened."
              : "Type a pattern to search the code."}
          </Empty>
        </Panel>
      ) : (
        repos.map((r) => (
          <section key={r} className="mb-6" aria-label={r}>
            {scoped ? null : <h2 className="mb-2 text-lg font-semibold text-fg-strong">{r}</h2>}
            <Panel>
              <Results repo={r} q={q} mode={mode} />
            </Panel>
          </section>
        ))
      )}
    </Page>
  );
}
