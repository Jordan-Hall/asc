// The command palette (Ctrl+K / Cmd+K): jump to a repository, a page or an action from the keyboard.
// A modal <dialog> (focus is trapped and Escape closes it natively) holding a WAI-ARIA combobox.
// Lazy chunk: the header only imports the open/close store.
import { type ReactNode, useEffect, useId, useMemo, useRef, useState } from "react";
import { useApp } from "../api/queries";
import { useToken } from "../lib/auth";
import { closePalette, fuzzy, usePalette } from "../lib/palette";
import { parseRepoName, useRecent } from "../lib/recent";
import { useNavigate, useParams } from "../lib/router";
import { useTheme } from "../lib/theme";
import { Icon, type IconName } from "./icons";

interface Command {
  id: string;
  group: string;
  label: string;
  /** Extra words that also match. */
  keywords?: string;
  hint?: string | undefined;
  icon: IconName;
  run: () => void;
}

const MAX_RESULTS = 12;

function useCommands(query: string, helpOpen: () => void): Command[] {
  const { auth } = useApp();
  const token = useToken(auth);
  const navigate = useNavigate();
  const { owner, repo } = useParams();
  const recent = useRecent();
  const { setPref } = useTheme();
  const here = owner && repo ? `/${owner}/${repo}` : undefined;

  return useMemo(() => {
    const go = (to: string) => () => navigate(to);
    const out: Command[] = [];
    const typed = parseRepoName(query);
    if (typed) {
      const name = `${typed.owner}/${typed.repo}`;
      out.push({
        id: `open:${name}`,
        group: "Open",
        label: `Open ${name}`,
        icon: "repo",
        run: go(`/${name}`),
      });
    }
    const q = query.trim();
    if (q && !typed) {
      out.push({
        id: "search",
        group: "Search",
        label: `Search code for “${q}”`,
        icon: "search",
        run: go(`${here ?? ""}/search?q=${encodeURIComponent(q)}`),
      });
    }
    for (const name of recent.slice(0, 8)) {
      out.push({
        id: `repo:${name}`,
        group: "Repositories",
        label: name,
        icon: "repo",
        run: go(`/${name}`),
      });
    }
    if (here) {
      const page = (id: string, label: string, to: string, icon: IconName, hint?: string) =>
        out.push({ id, group: `In ${owner}/${repo}`, label, icon, hint, run: go(to) });
      page("repo-code", "Code", here, "folder", "g c");
      page("repo-changes", "Changes", `${here}/changes`, "layers", "g p");
      page("repo-actions", "Actions", `${here}/actions`, "bolt", "g a");
      page("repo-files", "Go to file…", `${here}/search?mode=files`, "file", "t");
      page("repo-search", "Search this repository", `${here}/search`, "search");
      page("repo-settings", "Repository settings", `${here}/settings`, "gear");
    }
    out.push(
      {
        id: "go-home",
        group: "Go to",
        label: "Dashboard",
        icon: "repo",
        hint: "g h",
        run: go("/"),
      },
      {
        id: "go-inbox",
        group: "Go to",
        label: "Inbox",
        icon: "inbox",
        hint: "g i",
        run: go("/inbox"),
      },
      { id: "go-search", group: "Go to", label: "Search", icon: "search", run: go("/search") },
      {
        id: "go-settings",
        group: "Go to",
        label: "Settings",
        icon: "gear",
        hint: "g s",
        run: go("/settings"),
      },
      {
        id: "help",
        group: "Actions",
        label: "Keyboard shortcuts",
        icon: "keyboard",
        hint: "?",
        run: helpOpen,
      },
      {
        id: "theme-light",
        group: "Actions",
        label: "Theme: light",
        keywords: "appearance",
        icon: "sun",
        run: () => setPref("light"),
      },
      {
        id: "theme-dark",
        group: "Actions",
        label: "Theme: dark",
        keywords: "appearance",
        icon: "moon",
        run: () => setPref("dark"),
      },
      {
        id: "theme-system",
        group: "Actions",
        label: "Theme: match system",
        keywords: "appearance",
        icon: "system",
        run: () => setPref("system"),
      },
      token
        ? {
            id: "sign-out",
            group: "Actions",
            label: "Sign out",
            icon: "signout",
            run: () => auth.signOut(),
          }
        : { id: "sign-in", group: "Actions", label: "Sign in", icon: "user", run: go("/signin") },
    );
    return out;
  }, [query, recent, here, owner, repo, token, auth, navigate, setPref, helpOpen]);
}

function filter(all: readonly Command[], query: string): Command[] {
  const q = query.trim();
  if (!q) return all.filter((c) => c.group !== "Search").slice(0, MAX_RESULTS);
  const scored: { c: Command; s: number; i: number }[] = [];
  all.forEach((c, i) => {
    // The synthetic "open" and "search" rows always show; the rest must match.
    const s =
      c.group === "Open"
        ? -0.5
        : c.group === "Search"
          ? 100
          : Math.min(
              ...[fuzzy(q, c.label), fuzzy(q, c.keywords ?? ""), fuzzy(q, c.group)].map((v) =>
                v < 0 ? Number.POSITIVE_INFINITY : v,
              ),
            );
    if (Number.isFinite(s)) scored.push({ c, s, i });
  });
  scored.sort((a, b) => a.s - b.s || a.i - b.i);
  return scored.slice(0, MAX_RESULTS).map((x) => x.c);
}

export default function Palette(props: { onHelp: () => void }): ReactNode {
  const { query: initial } = usePalette();
  const dialog = useRef<HTMLDialogElement | null>(null);
  const input = useRef<HTMLInputElement | null>(null);
  const [query, setQuery] = useState(initial);
  const [active, setActive] = useState(0);
  const listId = useId();
  const helpOpen = props.onHelp;
  const all = useCommands(query, helpOpen);
  const results = useMemo(() => filter(all, query), [all, query]);
  const at = Math.min(active, Math.max(results.length - 1, 0));

  useEffect(() => {
    const d = dialog.current;
    if (d && !d.open) d.showModal();
    input.current?.focus();
    input.current?.setSelectionRange(initial.length, initial.length);
  }, [initial]);

  useEffect(() => {
    document.getElementById(`${listId}-${at}`)?.scrollIntoView({ block: "nearest" });
  }, [at, listId]);

  const run = (c: Command | undefined) => {
    if (!c) return;
    closePalette();
    c.run();
  };

  let lastGroup = "";
  return (
    <dialog
      ref={dialog}
      aria-label="Command palette"
      onClose={closePalette}
      onClick={(e) => {
        if (e.target === dialog.current) dialog.current?.close();
      }}
      className="fixed left-1/2 top-[10vh] m-0 w-[min(40rem,calc(100vw-1rem))] -translate-x-1/2 overflow-hidden rounded-2xl border border-line bg-surface p-0 text-fg shadow-2xl backdrop:bg-black/50"
    >
      <div className="flex items-center gap-3 border-b border-line px-4">
        <Icon name="search" className="shrink-0 text-fg-muted" />
        <input
          ref={input}
          role="combobox"
          aria-expanded="true"
          aria-controls={listId}
          aria-activedescendant={results.length > 0 ? `${listId}-${at}` : undefined}
          aria-autocomplete="list"
          aria-label="Search repositories, pages and actions"
          autoComplete="off"
          spellCheck={false}
          placeholder="Search repositories, pages and actions"
          value={query}
          onChange={(e) => {
            setQuery(e.target.value);
            setActive(0);
          }}
          onKeyDown={(e) => {
            if (e.key === "ArrowDown" || e.key === "ArrowUp") {
              e.preventDefault();
              const n = results.length;
              if (n > 0) setActive((at + (e.key === "ArrowDown" ? 1 : n - 1)) % n);
            } else if (e.key === "Enter") {
              e.preventDefault();
              run(results[at]);
            }
          }}
          className="h-14 min-w-0 flex-1 bg-transparent text-base text-fg-strong outline-none placeholder:text-fg-muted"
        />
        <kbd className="rounded border border-control px-1.5 font-mono text-xs text-fg-muted">
          Esc
        </kbd>
      </div>
      <div
        id={listId}
        role="listbox"
        aria-label="Results"
        className="max-h-[min(24rem,60vh)] overflow-y-auto p-2"
      >
        {results.length === 0 ? (
          <p className="px-3 py-8 text-center text-sm text-fg-muted">
            Nothing matches “{query.trim()}”. Type owner/repo to open a repository.
          </p>
        ) : null}
        {results.map((c, i) => {
          const heading = c.group !== lastGroup;
          lastGroup = c.group;
          return (
            <div key={c.id} role="presentation">
              {heading ? (
                <div
                  role="presentation"
                  className="px-3 pb-1 pt-2 text-xs font-semibold uppercase tracking-wide text-fg-muted"
                >
                  {c.group}
                </div>
              ) : null}
              <div
                id={`${listId}-${i}`}
                role="option"
                tabIndex={-1}
                aria-selected={i === at}
                onClick={() => run(c)}
                onPointerMove={() => setActive(i)}
                className={`flex min-h-10 cursor-pointer items-center gap-3 rounded-lg px-3 text-sm ${
                  i === at ? "bg-subtle-hover text-fg-strong" : "text-fg"
                }`}
              >
                <Icon name={c.icon} className="shrink-0 text-fg-muted" />
                <span className="min-w-0 flex-1 truncate">{c.label}</span>
                {c.hint ? (
                  <kbd className="rounded border border-control px-1.5 font-mono text-xs text-fg-muted">
                    {c.hint}
                  </kbd>
                ) : null}
              </div>
            </div>
          );
        })}
      </div>
      <div
        className="flex items-center gap-4 border-t border-line bg-subtle px-4 text-xs text-fg-muted"
        style={{ height: 36 }}
      >
        <span>
          <kbd className="font-mono">↑↓</kbd> move
        </span>
        <span>
          <kbd className="font-mono">Enter</kbd> open
        </span>
        <span className="ml-auto" role="status" aria-live="polite">
          {results.length} {results.length === 1 ? "result" : "results"}
        </span>
      </div>
    </dialog>
  );
}
